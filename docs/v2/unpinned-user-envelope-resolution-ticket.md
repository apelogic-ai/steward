# DP-S02: Resolve User Envelope authority without a product pin

Priority: P0 demo unblocker

Status: implementation in progress

## Goal

Remove User Envelope UID, revision, and digest pins from product-owned Git while
preserving Steward's exact, immutable authority evidence and every existing
revocation fence.

## Scope

- make `InvocationManifest.envelope` optional without changing frozen M1/v1;
- retain the existing exact-digest selection behavior when the field is present;
- when it is absent, resolve exactly one active provisioned Envelope owned by the
  authenticated canonical user;
- reject zero matches and reject multiple matches as ambiguous;
- expand omitted package `requires` to the resolved Envelope maximum and validate
  explicit requirements against that Envelope;
- continue independent admission against the current service Envelope;
- persist the resolved Envelope request UID, revision, digest, approved snapshot,
  effective requirements, and source closure through the existing Task reservation
  and evidence shape; and
- preserve exact-evidence idempotency and the existing pre-effect authority
  revalidation added by PR 78 and DP-S01.

This change adds no dependency, migration, CRD field, `steward-run` input, or new
desired-state path.

## Negative proofs

- no active provisioned User Envelope fails before Task reservation;
- multiple active provisioned User Envelopes fail as ambiguous before reservation;
- explicit requirements outside the resolved Envelope fail admission;
- a concurrent rotation or revocation loses the transactional reservation race and
  existing direct-Task fences prevent runtime or execution effects afterward; and
- an idempotent retry cannot replace the Task's originally resolved authority
  evidence with a newly active Envelope.

## Rollout

1. Merge and deploy the compatible Steward change.
2. Remove the `envelope` member and exact-digest machinery from the GitOps-owned
   invocation and demo tooling.
3. Keep UI provisioning of a bounded User Envelope, source authorization, exact
   package commits, invocation paths, and diagnostics unchanged.
4. Add explicit package `requires` independently; omission continues to mean the
   resolved Envelope maximum during this rollout.

## Deferred authority-lease design

This ticket does not make the admission snapshot irrevocable for the duration of a
run. Existing revocation and rotation revalidation remains authoritative until a
separate task-bound authority lease is designed. Mid-run non-disruptive rotation is
therefore explicitly deferred rather than approximated by weakening a lifecycle
check.
