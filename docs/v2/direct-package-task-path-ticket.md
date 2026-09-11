# DP-S01: Resolve and execute direct packages through Steward

Priority: P0

Status: implementation in progress from rebased checkpoint `5d6175c` on the reviewed
C01/G01 stack; DP-I01 is merged

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

## Test-preparation evidence

- branch: `feat/direct-package-task-path`, based on DP-C01 checkpoint `042e8aa`;
- local commit: `bf53e91`;
- five real `POST /v1/tasks` negative cases cover unauthorized source, wrong exact
  object, inactive Envelope, over-authority requirements, and cross-repository
  `git:trigger`;
- every case currently fails at the missing v2 request boundary before Task
  reservation, as intended; and
- the frozen versioned-v1 route remains green.

This checkpoint is intentionally not pushed and has no PR while its tests are red.

## Implementation-readiness audit

- The five current cases compile, reach the real `POST /v1/tasks` route, and prove no
  Task is reserved, but all stop at the same missing-v2 deserialization boundary.
  After DP-G01, replace them with behavior-specific source and Identity fakes so each
  named negative reaches its own authorization or admission seam.
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
