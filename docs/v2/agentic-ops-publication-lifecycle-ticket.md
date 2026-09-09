# Ticket: Automate customer-authored artifact publication and release provenance

Status: follow-up implementation may start before PR #78 release; not a P0 demo prerequisite

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

- Package tooling and publication design may start immediately against frozen contracts.
  PR #78 merge or release is not a blanket start prerequisite.
- The frozen [`steward.m1/v1` contract](../contracts/m1/v1/README.md) remains the
  publication and artifact authority.
- Coordinate package layout with the owner of the [P0 demo](agentic-ops-local-demo-ticket.md).
  Reuse its artifact where compatible; the completed demo is not a start prerequisite.
- Before live publication, implement and verify publisher authorization, private-source
  retrieval, repository/catalog bindings, create-only persistence, and witness APIs.
  Frozen JSON schemas alone do not provide these server capabilities.
- Final GHA acceptance additionally needs a compatible M1 resolver, Identity claims,
  input-receipt path, and steward-run transport. Record exact component revisions and
  contract versions. The existing v0.4 name@version transport is separate compatibility
  coverage and cannot prove qualified M1 catalog resolution.
- Integration using #78 behavior requires a fixed candidate revision and passing
  applicable regression/E2E gates. Released-stack acceptance waits for compatible releases.

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

- Define and verify the publisher authentication and authorization handoff before
  automating writes. Task execution authority does not itself grant publication rights;
  never automate by copying browser sessions or provider credentials.
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
- a moved tag attempts to change recorded source identity (the exact original commit
  remains valid if its verified bytes are still available);
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

This ticket can run in parallel with the P0 demo and
[conformance ticket](customer-authored-task-conformance-ticket.md). P0 has priority.
Agree on package layout, manifest/lock versions, digest rules, publication authorization,
catalog/source bindings, request/response versions, and transport pins before integration.
Assign one owner to shared catalog/resolver/store code and one owner to agentic-ops
package paths. Use separate branches; coordinate edits rather than rewriting shared work.
The P0 coordinator schedules isolated development tests and hands demo requirements
to GitOps. GitOps owns local-main deployment, readiness, and the agreed rehearsal
window; development workers must not use it for testing or recovery. This ticket
must not upgrade or operate the demo stack independently.
Manual fixtures may unblock B only when they exercise the same contract; copying a
legacy prompt is not a substitute for M1 publication/resolution coverage.
