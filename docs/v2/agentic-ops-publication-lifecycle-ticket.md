# Ticket: Optional catalog publication and release provenance

Status: reprioritized; not required by the proposed direct-package execution path

## Why this ticket changed

The original ticket made automated Steward catalog publication the production path for
every customer-authored TaskDefinition. The proposed
[direct-package architecture](direct-package-task-invocation.md) instead treats reviewed,
ratified Git source as directly invocable and makes publication optional.

This ticket now owns distribution and discovery for customers that want a curated
catalog, reusable cross-repository coordinates, or a separate organizational promotion
decision. It must not block same-repository GHA execution.

`agentic-ops` PR #1 already delivered offline package-bundle and GitHub release/tag
provenance tooling. That tooling validates candidates; it is not a live Steward
publisher, an authorization mechanism, or an authenticated publication witness.

## Goal

Provide an optional, machine-verifiable path in which an authorized publisher promotes
an already valid immutable package resolution into a create-only Steward catalog
coordinate. The catalog aliases exact source and closure identity; it never replaces or
weakens direct-source validation.

## Dependencies

- The frozen `steward.m1/v1` publication contract remains unchanged for its consumers.
- Any new catalog integration with direct packages waits for approval of the direct
  package architecture and its versioned source-resolution contract.
- Reuse the direct resolver's package validation, source authorization, immutable blob,
  and closure-digest implementation. Do not create a second resolver.
- Define publisher authentication and catalog/source bindings before enabling writes.
- No AgentSession, AgentInstance, TaskGraph, DEV deployment, or stable-lane change is a
  dependency.

## Scope

### `agentic-ops`

- Preserve deterministic package and closure validation against exact Git commits.
- Produce optional human-readable GitHub release/tag metadata bound to an exact commit
  and digest. Tags and releases remain discovery metadata, never authority or identity.
- Retain independent negative tests for moved tags, repository mismatch, dangling or
  cyclic annotated tags, draft releases, unsafe paths, and changed bytes.

### Steward catalog promotion

- Authenticate a distinct publisher role only where the customer requests a separate
  promotion boundary. Task execution authority does not imply publication authority.
- Promote an existing verified direct-package resolution or perform the same resolver
  operation once, then bind a qualified coordinate to the exact repository ID, commit,
  path, content digest, and closure digest.
- Preserve create-only and idempotent behavior. An existing coordinate is never rebound,
  even if the requested source or bytes appear equivalent.
- Return a non-secret witness suitable for later Task evidence and independent checking.
- Never dispatch a Task, mutate an Envelope, grant authority, or activate a runtime as
  a publication side effect.

## Required negative tests

- mutable branch, tag, release, or URL used as artifact identity;
- moved tag or release metadata attempting to change the exact source commit;
- path escape, symlink, submodule, undeclared dependency, or digest mismatch;
- unauthorized repository, catalog, source root, or publisher;
- idempotency-key reuse with any changed immutable field;
- existing-coordinate republish or rebinding under a new key; and
- an attempted publication field selecting an Envelope, principal, credential,
  connection, runtime, image, native policy, namespace, endpoint, or runner label.

Every rejected case creates no witness and changes no Task or runtime state.

## Exit criteria

1. Repeated clean resolution of one reviewed package produces identical source and
   closure identity.
2. An authorized promotion creates exactly one coordinate/witness; an identical retry
   returns it without duplication.
3. All rebinding, source, digest, and authority-negative cases fail atomically.
4. Both a direct Task and an optional catalog-coordinate Task resolve to the same
   immutable package closure and common Task application path.
5. GitHub release/tag metadata can be checked independently but is never required to
   execute the direct package.
6. Relevant repository and cross-product gates pass with no unresolved warnings.

## Non-goals

- mandatory publication for customer-authored same-repository Tasks;
- duplicating direct package ingress or resolution;
- treating repository review, publication, Task admission, and execution as one action;
- demonstrating over-envelope approval or multiple-package isolation;
- DEV readiness or stable promotion; and
- AgentSession, AgentInstance, or TaskGraph behavior.

## Delivery boundary

Defer new live publisher work until the direct-package architecture is approved and
DP-C01 establishes compatible source and evidence fields. Offline provenance tooling
may continue independently without changing runtime contracts. When resumed, assign one
owner to shared resolver/catalog persistence and keep release automation in its own
repository branch. The direct GHA path and its conformance work have priority.
