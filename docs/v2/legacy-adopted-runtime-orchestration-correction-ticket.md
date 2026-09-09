# Correction ticket: preserve legacy adopted runtimes in durable Task orchestration

Status: accepted correction for PR #78

Target: `feat/v2-common-core-sprint`, existing PR #78

Architecture: [`../task-runtime-orchestration.md`](../task-runtime-orchestration.md)

## Problem

The durable Task orchestration implementation correctly removes lifecycle effects from
the API server, but it also rejects every legacy submission containing
`agentRuntimeUid`. That is a public compatibility regression, not a required consequence
of single-owner orchestration.

Before the refactor, the frozen legacy adapter used `agentRuntimeUid` to adopt an exact,
already-running `AgentRuntime`. Steward admitted only a runtime whose namespace, complete
spec, canonical principal, and approval state matched the resolved Task. Execution reused
that runtime, and Task finalization detached without deleting it.

The behavior is consumed by the pinned `steward-run` v0.4 compatibility lane:

- its `agent-runtime` input sends `agentRuntimeUid`;
- it requires the response ownership to be `adopted`;
- the frozen `steward.m1/v1` compatibility section accepts that legacy request shape
  through Steward v0.2.x; and
- Steward's full Task E2E proves execute-and-detach reuse of a standing runtime.

PR #78 currently contradicts those claims. It retains adopted-runtime database,
controller, and cleanup semantics while the API returns 422, and its generated OpenAPI
still advertises successful adoption.

## Decision

Preserve exact-UID adoption only in the frozen legacy adapter. M1 submissions continue to
reject caller-selected runtime identity.

The API server may perform a read-only lookup to resolve immutable submission inputs. It
must not create, bind, patch, activate, execute against, or delete the runtime. The
reconciler remains the sole owner of every lifecycle effect and independently observes the
exact UID before the Task becomes active.

`agentRuntimeUid` is therefore a legacy requested reference, not evidence that the Task is
bound. The authoritative observed UID remains the reconciler-written
`task_runtime_operations.runtime_uid`.

## Required implementation

1. Restore the legacy adapter's exact-UID lookup and admission validation without restoring
   API-side binding or any Kubernetes mutation.
2. Atomically reserve an adopted Task and operation with:
   - a server-generated Task UID and operation ID;
   - server-resolved namespace and name;
   - immutable `expected_runtime_uid` equal to the validated legacy reference;
   - immutable runtime spec and manifest digests; and
   - `runtime_uid = NULL` until reconciler observation.
3. Make an exact retry compare the request with `expected_runtime_uid`, including after a
   crash before observation. A different UID is an idempotency conflict.
4. Have the reconciler verify exact UID, namespace/name, complete spec, canonical owner,
   current authority, readiness, and observed spec digest before recording
   `runtime_observed` and `active`.
5. Activation of an adopted runtime records observation only. It never applies a Task-owned
   manifest or changes the shared runtime's providers, credentials, budget, TTL, or
   annotations.
6. Revalidate the adopted binding before execution. Drift, deletion, or same-name UID
   replacement fails closed and enters cleanup.
7. Finalization cancels the Task's execution attempt and retires Task-owned approval/grant
   projections, but never deletes the adopted runtime.
8. Keep the staged rollout fence and all six PR #78 lifecycle corrections intact.

## Compatibility boundary

- Legacy v0.2 compatibility requests may contain `agentRuntimeUid` and may return
  `runtimeOwnership: adopted`.
- A newly reserved adopted Task returns 202 while the reconciler observes the exact UID.
- Every legacy adopted response projects the immutable server-validated target
  `runtimeUid`, as required by the pinned caller. This is a reference, not proof
  of durable binding or runtime readiness; persisted observed UIDs remain null
  until controller observation. Exact legacy submission retries retain 201/202,
  including finalized historical Tasks. M1 exact retries retain 200.
- M1 requests never contain or select a runtime UID.
- No name-only adoption, fallback lookup, replacement-UID recovery, or implicit adoption is
  permitted.
- Removal of the legacy field remains the separately frozen Steward v0.3.0 transition; this
  PR must not move that removal earlier.

## Required tests

- Legacy exact-UID submission reserves adopted intent without API-side binding.
- Legacy submission rejects an out-of-envelope, wrong-namespace, spec-mismatched,
  pending-approval, absent, or ambiguous runtime.
- An exact retry before and after UID observation is idempotent.
- A retry with a different UID conflicts without changing durable state.
- M1/versioned submissions reject `agentRuntimeUid`.
- Two reconcilers converge on one exact adopted binding.
- Same-name replacement with another UID fails closed.
- Adopted activation performs no runtime write.
- Execution revalidates the exact shared binding.
- Cancellation/finalization retires Task-owned projections and does not issue a Kubernetes
  delete for the adopted runtime.
- The existing full Task E2E continues to create a standing runtime, adopt it, execute, detach,
  and prove the standing runtime survives.

## Acceptance criteria

- The frozen legacy `steward-run` adoption path remains functional through v0.2.x.
- The M1 server-owned runtime identity boundary is unchanged.
- The API server performs no adopted-runtime lifecycle effect.
- No authority or execution is enabled before exact UID observation.
- Retry, replacement, authority-expiry, cancellation, and finalization paths converge after
  crashes and concurrent reconciliation.
- The existing adoption E2E is retained and green.
- Focused tests, workspace tests, the isolated Task E2E, and `cargo xtask ci` are green.
- The completed correction is committed and pushed to the existing PR branch; the PR is not
  merged or closed by an agent.

## Non-goals

- General caller-selected runtime support in M1.
- Name-based or label-based adoption.
- Resident-agent dispatch protocol design.
- Mutating a shared runtime to satisfy a Task.
- Extending legacy compatibility beyond the frozen v0.3.0 removal boundary.
