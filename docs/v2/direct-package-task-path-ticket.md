# DP-S01: Resolve and execute direct packages through Steward

Priority: P0

Status: focused red checkpoint retained locally at `bf53e91`; implementation depends
on DP-G01 and DP-I01

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
