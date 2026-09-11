# DP-S01: Resolve and execute direct packages through Steward

Priority: P0

Status: merged by `apelogic-ai/steward#85` as `3d6a76b`; reviewed head `99c5e0b`
on merged G01 `8c2ca03` passed all required gates

## Goal

Add direct Git package resolution to Steward while preserving the one-admission-library
and common-Task-service invariants established by PR 78.

## Scope

- accept `invocation-path` plus Identity-ratified trigger provenance;
- authorize the caller and package repositories through stable external identities;
- fetch and validate the invocation manifest and package closure through DP-G01;
- resolve `git:trigger` only for the invoking repository;
- find an active caller-authorized Envelope by approved content digest;
- expand omitted `requires` to the Envelope maximum and admit explicit requirements;
- render instruction-only skills without introducing executable skill behavior;
- compute and persist closure, source, authority, Principal, input, runtime, and output
  evidence;
- reserve idempotently and invoke the common Task application service; and
- when opted in, capture successful coding-agent stdout and stderr into reserved Task
  output entries without logging injected credentials.

There is no legacy/catalog fallback in the v2 handler. Frozen v1 behavior remains on
its existing route and resolver.

## Regression and negative tests

- start with unauthorized-source, wrong-object, inactive-Envelope, over-authority, and
  trigger-repository mismatch tests;
- prove resolution and admission complete before Task reservation or runtime creation;
- prove retries cannot switch source bytes, closure digest, Envelope, or diagnostics;
- prove revoked source or Envelope authority cannot reactivate through a retry;
- prove provider-control execution can never enable full task-I/O logging;
- prove omitted skills runs with no skills; and
- prove v1 requests retain their current behavior.

## Exit criteria

- the same common application service owns direct and existing Task orchestration;
- successful same-repository and cross-repository Tasks bind exact evidence;
- real runtime execution returns declared outputs and optional transcript files;
- all focused, integration, and repository gates are green; and
- no local-main environment was used during development.

## Parallel boundary

Test scaffolding may start after DP-C01. Source-backed implementation waits for DP-G01.
Coordinate final integration with DP-I01 and DP-R01; serialize heavy integration lanes.

## Implementation evidence

- branch: `feat/direct-package-task-path`, based directly on merged DP-G01 `8c2ca03`;
- local candidate head: `99c5e0b`;
- five real `POST /v1/tasks` negative cases cover unauthorized source, wrong exact
  object, inactive Envelope, over-authority requirements, and cross-repository
  `git:trigger`;
- every negative reaches its named authorization or admission seam and proves zero
  Task reservations and zero runtime operations;
- the successful path resolves exact prompt, skill, and asset content, computes the
  canonical closure digest, expands omitted skills to none, and reserves immutable
  direct-package evidence;
- the additive immutable evidence migration and reservation/idempotency checks are
  implemented;
- the production GitHub source adapter, bounded stable-ID caller-to-source catalog,
  read-only App-key mount, and conditional GitHub API egress are wired fail closed;
- the generated API client is current;
- successful full diagnostics persist bounded stdout and stderr under only the two
  reserved output paths before the atomic success marker; and
- `cargo xtask ci`, five real-Postgres regressions, generated-client verification,
  and the authenticated OpenShell `0.0.98` runtime E2E are green. The disposable E2E
  cluster, containers, networks, state, kubeconfig, and processes were verified
  absent afterward; local-main and stable were untouched.

The implementation was merged through `apelogic-ai/steward#85` as `3d6a76b`.

## Implementation-readiness audit

- The five initial cases now use behavior-specific source and Identity fakes and
  reach their own authorization or admission seams.
- Add one nullable, immutable direct-package evidence JSON object through a new
  additive migration. Do not repurpose or relax the existing all-or-none workflow and
  User-Envelope pin columns, and do not edit migration history.
- Reservation and idempotent retry compare the complete immutable evidence while
  accounting for the server-generated winning Task UID. Database constraints and a
  separate update trigger reject malformed or mutated evidence.
- Revalidate the exact selected User Envelope before reservation and every runtime or
  execution effect. A stale selection terminalizes the Task and cleans any inert
  runtime; it must not create a repeating queued-work loop or later reactivate.
- Source authorization precedes content trust. The demo's explicit `git:sha1`
  `gitops` to `agentic-ops` read requires an active caller-to-source binding;
  `git:trigger` remains same-repository-only.
- Direct v2 request and status types are additive unions around the unchanged legacy
  API. Regenerate the web API client through the repository command; never hand-edit
  generated files.
- Successful full diagnostics persist exact bounded stdout and stderr in the
  adapter-owned attempt state before its success marker, then add only the two
  reserved diagnostic files to the existing output archive. Diagnostics-off,
  provider-control, failure, overflow, or interrupted postprocessing must not expose a
  transcript or report success.
- This design needs no new S01 dependency, CRD change, `steward-mint` change, or
  existing-field semantic rewrite. The additive migration and normal client
  regeneration do not require an exception to repository rules.

## Recorded post-demo hardening debt

The P0 demo still requires transactional exact-Envelope validation before Task
reservation, revalidation before every resumable external runtime-create effect,
baseline activation validation, and the existing claim/start fences. The following
additional adversarial coverage is explicitly deferred until after the demo; this
deferral does not relax those runtime checks:

- exhaustively exercise Envelope replacement at `RuntimeObserved`,
  `ApprovalPending`, both `ActivationPending` windows, and after the durable start
  linearization point;
- add a bounded concurrent User-Envelope replacement/service-Envelope revision test
  that proves the User-then-service lock order cannot deadlock;
- expand malformed persisted-evidence coverage across every identity, revision,
  digest, and approved-snapshot field;
- prove journal-event uniqueness and the absence of duplicate operation, runtime,
  attempt, or lease allocation across every stale-worker retry stage; and
- extend mid-lifecycle source-authorization revocation tests beyond the P0
  pre-admission boundary.
