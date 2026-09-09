# Ticket: Expand customer-authored GHA Task conformance

Status: preparation and isolated implementation may start before PR #78 release; P0 has priority

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

- Fixtures, test design, and isolated harness implementation can start before #78 merge
  or release. Tests of #78 behavior require an exact candidate revision and its applicable
  regression/E2E gates; do not imply a fully tested release from candidate results.
- The [P0 demo](agentic-ops-local-demo-ticket.md) owns the initial repository, caller,
  publication mapping, local-main readiness, and rehearsals. Reuse its baseline when
  available; do not wait for it to begin independent preparation.
- The exact GHA caller uses a pinned `steward-run` reusable workflow and is registered
  by stable repository ID plus exact workflow identity.
- Maintain separate legacy v0.4 and M1 test matrices. Legacy name@version publication
  accepts agent/prompt and derives runtime requirements from the User Envelope. It does
  not prove package-requirement enforcement, qualified catalog resolution, or M1 evidence.
- M1 cases require implemented catalog publication/resolution, Identity catalog/source
  claims, envelopeRef and input-receipt handling, and compatible steward-run transport.
  Record each providing implementation and exact revision before running those cases.
- Manually pre-published fixtures are allowed only within their actual contract. Missing
  M1 capabilities are unmet dependencies, not passing or silently skipped coverage.
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
- Prove an over-envelope TaskDefinition is parked unchanged with a deterministic delta.
  An inert AgentRuntime may exist to bind approval to its exact UID, as required by #78.
  No task execution, model/tool provider attachment, credential projection, inference
  authorization, or external-network authority is activated while it waits.
- Prove approval is instance-bound, time-bounded, and revalidated against the current
  grant, Envelope revision, artifact closure, and deployment binding before activation.
- Prove rejection, expiry, revocation, or a changed Envelope leaves no reusable approval
  authority and reaches an explicit terminal or newly evaluated state without a retry
  loop.

### Multiple-workflow isolation

- Prove a coordinate resolves exactly its authorized locked closure. Explicitly locked
  and authorized shared dependencies are allowed; undeclared substitutions fail.
  Approval, Task state, evidence, and runtime identity cannot cross Task ownership.
- Prove same-name artifacts in unauthorized catalogs are not searched or selected.
- Prove queued and running Tasks retain their admitted coordinate and closure when a new
  artifact version is published.
- Prove portable artifacts cannot choose identity, credentials, connections, runtime UID,
  image, native policy, namespace, endpoint, or runner label. Operator transport config
  owns runner/endpoint settings. M1 envelopeRef is a protected transport field that
  Steward checks against the authenticated principal's issued Envelopes; reject foreign
  or caller-injected overrides without forbidding the legitimate field.

### Stable-lane acceptance

- Execution waits for compatible released component pins and explicit human approval
  for any required stable promotion. Preparation can proceed before release. Existing
  stable pins lacking M1 support cannot satisfy M1 acceptance.
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
- missing, unknown, or unauthorized coordinate; unqualified coordinates rejected for
  M1 while the explicitly supported legacy shape remains accepted in its own lane;
- undeclared/unauthorized dependency substitution or reuse of another Task's approval;
- over-envelope submission creating any effectful runtime state before approval;
- expired or revoked approval reused after a retry or controller restart;
- Envelope or deployment-binding revision changed before runtime activation;
- artifact version published while an older Task is queued or running; and
- successful, failed, rejected, expired, and cancelled Tasks leaving runtime-owned
  resources behind.

## Exit criteria

1. Two customer-authored TaskDefinitions execute independently through the pinned GHA
   path in local-main.
2. The over-envelope negative path remains inert without execution authority, and approve, reject,
   expiry, revocation, and stale-authority cases have deterministic outcomes.
3. Unauthorized cross-workflow/catalog substitutions fail before execution; authorized
   locked shared dependencies remain usable.
4. The required positive and negative matrix passes in local-stable against pinned
   released artifacts.
5. Every successful positive agentic scenario performs real model inference and an
   authorized MCP call. Rejected and parked scenarios prove absence of unauthorized
   execution and calls. Logs and artifacts contain no credential material.
6. After finalization, no disposable Task-owned runtime, Sandbox, UID-scoped Secret,
   or Task credential projection remains. Final suite teardown revokes run-created OAuth
   state and model projections; pre-existing user connections are not Task-owned.
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

Use separate branches and assign ownership before editing shared fixtures, catalog,
resolver/store code, Identity configuration, or caller workflows. P0 owns the demo
package and caller; A owns publication tooling and the catalog integration handoff;
B owns additional test scenarios and harnesses. Reuse agreed transport and manifest
versions, digest rules, source/catalog bindings, and publisher authorization.
Only the coordinator may schedule shared local-main deployment/configuration changes,
credentials, dispatches, or heavy tests. Stable execution follows a green main matrix
and compatible release/promotion; heavy lanes never overlap. A legacy manual fixture
cannot stand in for an M1 publication witness or catalog-scoped admission.
