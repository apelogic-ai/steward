# Ticket: Expand customer-authored GHA Task conformance

Status: post-PR #78 follow-up; may start with manually pre-published fixtures

## Problem

The initial `agentic-ops` demonstration intentionally proves one happy-path artifact in
one local lane. It does not prove how the customer DevInt/DevOps boundary behaves when
authority is insufficient, when a catalog contains multiple TaskDefinitions, or when a
released stack consumes the same artifacts in the retained stable lane.

Those cases cross Steward, Identity, `steward-run`, GitOps, OpenShell, MCP-GW, and
LiteLLM. Combining them with artifact-publication automation would create one ticket
with unrelated ownership, slow diagnosis, and an unnecessary serial dependency.

## Goal

Prove that a customer DevInt/DevOps role can author multiple portable TaskDefinitions,
invoke exact approved versions through the normal GitHub Actions integration, and
receive deterministic admission outcomes without acquiring control of identity,
Envelopes, credentials, provider connections, runtimes, or deployment policy.

## Dependencies

- PR #78 is merged and its common Task runtime orchestration guarantees remain green.
- The single-artifact local-main demo is recorded as the happy-path baseline.
- The exact GHA caller uses a pinned `steward-run` reusable workflow and is registered
  by stable repository ID plus exact workflow identity.
- Manually pre-published fixtures are allowed during parallel development.
- Final automated-publication coverage consumes the immutable output of
  [`agentic-ops-publication-lifecycle-ticket.md`](agentic-ops-publication-lifecycle-ticket.md).

AgentSession, AgentInstance, TaskGraph, and DEV readiness are explicitly unnecessary.

## Scope

### Customer authoring and caller boundary

- Exercise at least two distinct TaskDefinitions owned by `agentic-ops`, each with an
  exact immutable coordinate and independently computed authority requirements.
- Invoke each through the normal `steward-run` GHA path. Tests must not synthesize a
  direct Steward API submission.
- Register only exact, manually dispatched caller workflows for the local testbed.
  Repository names, workflow inputs, branch names, or customer-authored manifests never
  substitute for the stable provider IDs and ratified workflow claims supplied by
  Identity policy.
- Keep the dedicated self-hosted runner single-concurrency and local-only, without DEV,
  AWS, or ambient-kubeconfig authority.

### Authority outcomes

- Prove an in-envelope TaskDefinition is admitted and executed without an approval
  record widening its authority.
- Prove an over-envelope TaskDefinition is parked unchanged with a deterministic delta;
  no runtime, provider attachment, credential projection, inference authorization, or
  external-network authority exists while it waits.
- Prove approval is instance-bound, time-bounded, and revalidated against the current
  grant, Envelope revision, artifact closure, and deployment binding before activation.
- Prove rejection, expiry, revocation, or a changed Envelope leaves no reusable approval
  authority and reaches an explicit terminal or newly evaluated state without a retry
  loop.

### Multiple-workflow isolation

- Prove that one coordinate cannot resolve another TaskDefinition's prompt, dependency,
  requirements, approval, Task state, evidence, or runtime.
- Prove same-name artifacts in unauthorized catalogs are not searched or selected.
- Prove queued and running Tasks retain their admitted coordinate and closure when a new
  artifact version is published.
- Prove a workflow input cannot choose an Envelope, principal, credential, connection,
  runtime UID, image, native policy, namespace, endpoint, or runner label.

### Stable-lane acceptance

- Repeat the final positive, isolation, and authority-negative scenarios against the
  retained local-stable lane using its pinned released artifacts and its own provisioned
  Envelope and connection state.
- Never copy templates, Envelopes, OAuth state, database files, credentials, or runtime
  state between local-main and local-stable.
- Use the explicit lane kubeconfig and context for every Kubernetes operation and run
  only one heavy local integration/E2E lane at a time.
- A stable result is evidence only for stable; a main result is evidence only for main.

## Required negative tests

- unregistered repository, workflow, ref, event, or reusable-workflow identity;
- unqualified, missing, unknown, or unauthorized TaskDefinition coordinate;
- workflow A attempting to use workflow B's dependency or approval;
- over-envelope submission creating any effectful runtime state before approval;
- expired or revoked approval reused after a retry or controller restart;
- Envelope or deployment-binding revision changed before runtime activation;
- artifact version published while an older Task is queued or running; and
- successful, failed, rejected, expired, and cancelled Tasks leaving runtime-owned
  resources behind.

## Exit criteria

1. Two customer-authored TaskDefinitions execute independently through the pinned GHA
   path in local-main.
2. The over-envelope negative path parks without side effects, and approve, reject,
   expiry, revocation, and stale-authority cases have deterministic outcomes.
3. Cross-workflow and cross-catalog substitution attempts fail before execution.
4. The required positive and negative matrix passes in local-stable against pinned
   released artifacts.
5. Every agentic run performs a real model call and at least one authorized MCP tool
   call, while logs and artifacts contain no credential material.
6. After every run there is no AgentRuntime, Sandbox, UID-scoped runtime Secret, model
   key projection, or retained local OAuth state owned by that run.
7. The relevant repository gates and affected integration/E2E targets are green with no
   warnings.

## Non-goals

- demonstrating repository PR review or attended publication approval;
- defining or implementing AgentSession, AgentInstance, or TaskGraph;
- DEV deployment, readiness, promotion, or fallback;
- broadening a caller allowlist to an organization, mutable repository name, arbitrary
  branch, `pull_request`, or unreviewed workflow; and
- running local-main and local-stable heavy lanes concurrently.

## Parallel delivery boundary

Identity/GitOps/`steward-run` caller registration, multiple-workflow fixtures, and
authority-state tests can proceed with manually pre-published artifacts while the
publication-lifecycle ticket is implemented. The shared integration point is the frozen
qualified coordinate plus publication witness. Stable-lane execution starts only after
the local-main matrix is green and never runs concurrently with it.
