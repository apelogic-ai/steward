# DP-S01: Resolve and execute direct packages through Steward

Priority: P0

Status: red server-path tests active; implementation depends on DP-G01

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

## Integration seam map

The first red checkpoint is based on DP-C01 commit
`042e8aa96a66d8927b50dc253509a5d437a5eeec`. It exercises the existing
`POST /v1/tasks` boundary with the frozen direct-submission body and proves that the
five initial escape cases must fail before the fake Task ledger observes a
reservation.

DP-S01 should integrate through these existing boundaries:

1. extend the authenticated Task identity with DP-I01's validated
   `SourceProvenance`; do not accept provenance in the request body;
2. add the DP-G01 provider-neutral source resolver to `TaskApiConfig` and use it to
   resolve the invocation manifest and package closure before reservation;
3. add caller-to-repository authorization and active Envelope-by-digest lookup to the
   Task ledger boundary, keeping stable external repository IDs at the authorization
   edge;
4. translate the resolved `DirectTaskDefinition` into the existing
   `AgentRuntimeSpec`, execution adapter request, admission decision, and
   `TaskReservationRequest` path instead of adding another runtime write path; and
5. persist the complete `DirectTaskBindingEvidence` and snapshotted diagnostics with
   the reservation before exposing the existing input, execute, poll, output, and
   finalization routes.

The current red test uses scenario-named invocation paths until DP-G01's fake source
port is available. On integration, those paths must resolve to deterministic fake Git
objects; they must not become special production values. Retry immutability,
revocation, omitted-skills, diagnostics exclusion, successful execution, and exact
evidence assertions remain to be added after the port and persistence shapes land.
