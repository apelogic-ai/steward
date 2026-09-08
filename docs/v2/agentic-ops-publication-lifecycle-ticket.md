# Ticket: Automate customer-authored artifact publication and release provenance

Status: post-PR #78 follow-up; ready after the single-artifact local demo baseline is recorded

## Problem

The first `agentic-ops` demonstration may use one pre-created, pre-approved artifact
that an administrator publishes to Steward before the attended run. That is sufficient
to prove the customer DevInt/DevOps authoring boundary and the normal GitHub Actions
Task path, but it leaves publication as an attended compatibility bridge.

A durable customer workflow needs a machine-verifiable path from reviewed source to a
create-only Steward catalog publication. Mutable branch names, tags, releases, copied
prompts, and administrator browser sessions must not become authority or artifact
identity.

## Goal

Implement the production-shaped artifact lifecycle in which:

1. a customer DevInt/DevOps author changes a portable package in `agentic-ops`;
2. repository checks validate the complete package and deterministic dependency closure;
3. release metadata identifies an exact source commit and content digests;
4. an authorized publisher submits the frozen M1 catalog publication request; and
5. Steward records one immutable publication witness or rejects the request without
   partial state.

This ticket automates publication. It does not make publication approval, Task
admission, or runtime activation the same operation.

## Dependencies

- PR #78 is merged and its common Task runtime orchestration guarantees remain green.
- The frozen [`steward.m1/v1` contract](../contracts/m1/v1/README.md) remains the
  publication and artifact authority.
- The single-artifact `agentic-ops` local demo establishes the initial package layout,
  exact GHA caller, and manually published compatibility fixture.

No AgentSession, AgentInstance, TaskGraph, DEV deployment, or stable-lane change is a
dependency.

## Scope

### `agentic-ops`

- Define the canonical versioned layout for TaskDefinition, prompt, instruction-only
  skill, Agent, dependency lock, execution requirements, and authority requirements.
- Validate manifests against the frozen M1 schemas and validate exact Markdown bytes.
- Resolve every dependency within the package root and compute deterministic content
  and closure digests.
- Produce publication metadata bound to the stable GitHub repository ID, exact 40-hex
  commit, root-contained source path, qualified coordinate, and digests.
- Publish a release marker only as human-readable discovery metadata. A GitHub tag or
  release name is never resolved by Steward and never replaces the commit or digest.
- Retain independent positive and negative artifact tests.

### Publication automation

- Use an explicitly authorized publisher identity; do not reuse a browser session,
  acting-user credential, provider token, runtime workload identity, or GHA task token.
- Submit the canonical `catalogPublicationRequest` with an idempotency key derived from
  immutable publication inputs.
- Treat the returned `catalogPublicationWitness` as the publication result and retain
  only non-secret evidence suitable for later verification.
- Retry only the byte-identical canonical request. A changed request under the same key
  is an idempotency conflict, not a new attempt.
- Never dispatch a Task, mutate an Envelope, approve an authority delta, or activate an
  AgentInstance as a side effect of publication.

### Steward

- Verify the configured source repository binding, exact commit, root-contained path,
  artifact bytes, content digest, and closure digest before creating a witness.
- Preserve create-only coordinate semantics for identical republish, changed source,
  changed digest, and source rebinding attempts.
- Expose enough non-secret result data for the publisher to distinguish a successful
  retry, coordinate conflict, integrity failure, and authorization failure.

## Required negative tests

- a mutable branch or tag is supplied where an exact commit is required;
- a tag is moved after release metadata is created;
- a path escapes the configured package root;
- source bytes or a dependency differ from the submitted digest;
- the repository ID, commit, coordinate, content digest, or closure digest changes
  under a reused idempotency key;
- a new idempotency key attempts to republish or rebind an existing coordinate;
- an unauthorized repository or publisher attempts publication; and
- publication input attempts to select an Envelope, principal, credential, connection,
  runtime, native policy, image, endpoint, namespace, or runner label.

Every failed case creates no publication witness and changes no Task or runtime state.

## Exit criteria

1. One reviewed `agentic-ops` package produces byte-identical publication metadata on
   repeated clean checkouts of the same commit.
2. The automated publisher creates exactly one witness, and an identical retry returns
   the same witness without a duplicate record.
3. Every required negative test passes against the real publication path.
4. A normal GHA Task can resolve the newly published immutable coordinate; publication
   itself does not dispatch that Task.
5. The relevant repository gates and cross-repository contract tests are green with no
   warnings.

## Non-goals

- demonstrating repository PR review or a browser approval ceremony;
- over-envelope Task approval;
- multiple-workflow isolation;
- stable-lane acceptance;
- DEV deployment or readiness;
- AgentSession, AgentInstance, or TaskGraph behavior; and
- treating GitHub releases or tags as authoritative artifact identity.

## Parallel delivery boundary

This ticket can run in parallel with
[`customer-authored-task-conformance-ticket.md`](customer-authored-task-conformance-ticket.md).
The conformance ticket may use manually pre-published fixtures until this ticket freezes
the publication metadata and witness handoff. Its final automated-publication scenario
depends on that handoff, not on this ticket's internal implementation.
