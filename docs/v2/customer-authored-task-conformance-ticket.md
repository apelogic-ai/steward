# Ticket: Expand customer-authored direct-package GHA Task conformance

Status: test design may continue; direct-path implementation waits for architecture and
contract approval

## Problem

The initial `agentic-ops` demonstration proves one compatibility-path Task. It does not
prove direct immutable package resolution, insufficient-authority behavior, isolation
between multiple customer packages, or released-stack behavior in the retained stable
lane.

Those cases cross Steward, Identity, `steward-run`, GitOps, OpenShell, MCP-GW, and
LiteLLM. They must test one agreed contract rather than using legacy publication as a
substitute for direct-source behavior.

## Goal

Prove that a customer DevInt/DevOps role can author multiple portable TaskDefinitions in
an authorized repository, invoke each from its exact ratified Git commit through normal
GHA, and receive deterministic admission outcomes without controlling identity,
Envelopes, credentials, provider connections, runtimes, or deployment policy.

## Dependencies

- PR #78 is merged and supplies the common Task application and orchestration core.
- The [direct-package architecture](direct-package-task-invocation.md) and DP-C01 wire,
  source, package, and evidence fixtures must be approved before implementing direct-path
  acceptance assertions.
- DP-A01, DP-R01, DP-I01, and DP-S01/S02 provide compatible package, transport,
  identity, resolver, admission, and evidence candidates.
- The [P0 demo](agentic-ops-local-demo-ticket.md) owns the existing compatibility
  artifact and attended local-main result. Reuse its scenario where useful, but do not
  represent its legacy `name@version` publication as direct-package evidence.
- Optional catalog publication is a separate matrix and is not required for these tests.
- AgentSession, AgentInstance, TaskGraph, and DEV readiness are unnecessary.

## Scope

### Customer authoring and caller boundary

- Exercise at least two packages in `agentic-ops`, each selected by an allowed relative
  path and bound to the exact ratified repository ID and triggered commit.
- Invoke each through the pinned `steward-run` reusable workflow. Tests must not replace
  the GHA source/identity boundary with a direct API fixture.
- Prove the trusted transport captures package bytes from the exact checkout itself and
  does not accept an authoritative archive produced by caller-controlled preparation
  steps.
- Keep repository/workflow/ref/event/reusable-workflow authorization based on stable IDs
  and ratified claims. Mutable display names and caller-supplied IDs never authorize.

### Source and package resolution

- Prove Steward computes and persists the complete root-contained closure before Task
  reservation and never follows a mutable ref afterward.
- Prove package and Task-input receipts are distinct, principal/source-bound, immutable,
  and non-substitutable.
- Prove an advancing `main` does not change queued, parked, running, retried, or recovered
  Tasks.
- Prove unsupported cross-repository imports fail rather than being copied or resolved
  through ambient provider credentials.

### Authority outcomes

- Prove an in-envelope package is admitted and executed without an approval record.
- Prove a foreign, inactive, revoked, expired, rebound, or digest-mismatched Envelope
  fails before runtime creation without revealing another principal's state.
- Prove an over-envelope package is parked unchanged with a deterministic delta. Any
  inert placeholder remains unable to use model, tool, credential, or network authority.
- Prove approval is exact-runtime-bound and revalidated against the package closure,
  active grant, Envelope revision/digest, and deployment binding before activation.
- Prove rejection, expiry, revocation, or relevant drift has a deterministic terminal or
  newly evaluated outcome and never enters a create/delete retry loop.

### Multiple-package isolation

- Prove each package resolves exactly its own locked closure. Explicit identical bytes
  may be content-addressed internally, but authorization, receipt ownership, Task state,
  approval, evidence, and runtime identity never cross boundaries.
- Prove same paths or names in another repository or commit are not searched or selected.
- Prove package content cannot choose identity, Envelope ownership, credentials,
  connections, runtime UID, image, native policy, namespace, endpoint, CA path, or runner
  label.
- Preserve a separate legacy and frozen-M1 compatibility matrix under their own contract
  discriminators.

### Stable-lane acceptance

- Development runs use isolated, run-owned environments and one heavy lane at a time.
- Stable execution waits for compatible released component pins and explicit human
  promotion. Existing stable pins without the new contract cannot satisfy acceptance.
- Repeat the positive, package-isolation, authority-negative, recovery, and cleanup
  scenarios in local-stable using that lane's own Envelope and connection state.
- Never copy Envelopes, OAuth state, databases, credentials, runtime state, or kubeconfig
  between local-main and local-stable. Evidence remains lane-specific.

## Required negative tests

- unregistered repository, workflow, ref, event, or reusable-workflow identity;
- caller-supplied or mismatched repository ID, commit SHA, source root, or principal;
- mutable ref, path traversal, duplicate normalized archive path, link, submodule,
  device/FIFO, sparse entry, archive metadata path override, duplicate JSON key, invalid
  encoding, missing/extra dependency, or changed locked byte;
- package archive substituted by an earlier untrusted GHA job;
- package/input receipt consumed by another principal, source, Task, or retry;
- idempotency-key reuse with changed package, input, Envelope, or trigger;
- foreign, expired, revoked, rebound, or digest-mismatched Envelope;
- over-envelope submission creating effectful authority before approval;
- expired or revoked approval reused after retry or controller restart;
- package, Envelope, grant, or deployment-binding drift before activation; and
- successful, failed, rejected, expired, and cancelled Tasks leaving Task-owned runtime
  or provider projections behind.

## Exit criteria

1. Two direct customer-authored packages execute independently through pinned GHA in an
   isolated environment without catalog publication.
2. Steward evidence binds each result to exact source, package closure, input, Envelope,
   admitted authority, execution binding, Task/runtime identity, and cleanup.
3. All source, receipt, authority, substitution, drift, recovery, and idempotency
   negative cases fail at the intended boundary.
4. Every positive scenario uses real model inference and an authorized MCP call;
   rejected and parked scenarios prove absence of unauthorized calls.
5. The final matrix passes against compatible released artifacts in local-stable.
6. No disposable runtime, Sandbox, UID-scoped Secret, or Task authority projection
   remains after finalization. Run-created credentials are revoked without touching
   pre-existing user connections.
7. Direct, legacy, and frozen-M1 compatibility gates are green and contract-separated.

## Non-goals

- catalog publication, release/tag provenance, or cross-repository package imports;
- enforcing GitHub pull-request review rules inside Steward;
- AgentSession, AgentInstance, TaskGraph, DEV deployment, or DEV readiness;
- broad caller authorization by mutable repository name or organization; and
- concurrent heavy local-main/stable testing.

## Parallel delivery boundary

Before DP-C01 approval, work is limited to threat modeling, fixtures, and harness design.
After fixtures freeze, package-validator, transport, Identity, and package-ingress tests
can develop in separate repositories/worktrees. One owner controls shared Steward
resolver/store integration. The coordinator serializes heavy isolated lanes. GitOps owns
retained local-main/stable state and schedules any accepted promotion or rehearsal; no
development worker uses those lanes for ad hoc testing or recovery.
