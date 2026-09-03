# Ticket: Reconcile Steward documentation for M1 and long-running agents

Status: accepted documentation reconciliation decision record

## Problem

Steward's documentation describes several architectural baselines at once:

- the frozen `steward.m1/v1` contract;
- the M1 delivery plan;
- an older Workflow/Task design exploration;
- the original v0.1 roadmap and data-plane design;
- the long-running-agent solution overview; and
- later decisions separating portable agent artifacts from Steward governance and
  runtime ownership.

The documents use overlapping terms differently and sometimes mix implemented
behavior, frozen contracts, historical plans, and proposed post-M1 architecture. A
reader cannot reliably determine which statements are authoritative.

## Goal

Create a coherent documentation baseline that:

1. preserves the frozen M1 contract without semantic changes;
2. marks every relevant document as frozen, implemented, proposed, historical, or
   superseded;
3. defines the accepted post-M1 concepts for long-running agents, Tasks, sessions,
   and runtimes;
4. makes the Steward, `agentic-ops`, deployment-operator, and connector boundaries
   explicit; and
5. provides one navigable document index and one consolidated set of genuinely
   deferred implementation decisions.

This ticket changes documentation only. It must not claim that proposed post-M1 APIs
or resources are implemented or frozen wire contracts.

## Authority hierarchy

The reconciled documentation must establish this order:

1. `docs/contracts/m1/v1/**` is the sole normative M1 wire contract.
2. Current implementation documentation describes behavior already present in the
   repository.
3. `docs/v2/**` describes accepted post-M1 semantics and clearly identified future
   implementation work.
4. Historical roadmaps and design explorations provide context but are not current
   contracts.
5. When two documents disagree, the higher-ranked source controls.

Do not modify any schema, fixture, manifest, or semantic requirement under
`docs/contracts/m1/v1/`.

## Locked terminology

| Term | Meaning |
|---|---|
| `Agent` | Immutable portable catalog artifact describing an agent interface |
| `TaskDefinition` | Immutable definition of one governed execution atom |
| `Task` | One admitted invocation of a TaskDefinition |
| GitHub Actions workflow | A trigger and transport that may invoke one Steward Task |
| M1 `workflow` field | Frozen historical field name whose value resolves a TaskDefinition |
| `AgentInstance` | Persistent governed standing delegation for one canonical user |
| `AgentRuntime` | Replaceable execution incarnation of an AgentInstance, identified by exact runtime UID |
| `AgentSession` | Durable interactive conversation/attachment associated with an AgentInstance |
| `SessionRelay` | Typed streaming boundary consumed by TUI, web, or connector adapters |
| role implementation | Portable process or image implementing an agent or execution role |

Steward does not currently define a domain object named `Workflow`. The prior
multi-step Workflow proposal is a historical design exploration. If multi-Task
orchestration is reconsidered, it must use a separately accepted `TaskGraph`-like
contract rather than overloading GitHub Actions and frozen M1 terminology.

## Locked execution model

### Task atom and transport

`TaskDefinition` is the portable governed atom and `Task` is one invocation. A GitHub
Actions workflow is one possible transport:

```text
GitHub Actions workflow
        -> steward-run
        -> M1 workflow coordinate
        -> TaskDefinition
        -> Task
```

A normal GitHub Actions invocation creates a Task without creating an AgentSession.
The frozen M1 one-Task lifecycle remains unchanged.

### No TaskGraph in the accepted baseline

Long-running agents do not require a multi-Task graph scheduler. An AgentInstance
executes independently admitted Tasks. A future TaskGraph requires a separate
architecture and contract decision if Steward must own dependencies, per-node
authority, branching, durable inter-node pause/resume, or graph-level evidence.

## AgentInstance contract

`AgentInstance` is the persistent desired-state concept and will be CRD-backed. Its
desired state is written only through Steward admission; its current operational
status is written only by the controller. Postgres stores history, idempotency,
journal events, and UI projections, not authoritative current phase.

The exact post-M1 wire schema is not part of this documentation ticket. The following
semantic properties are locked:

- the canonical owner is server-authored;
- Agent, TaskDefinition, Envelope, and deployment bindings are immutable or
  revision-pinned;
- browser input cannot choose identity, runtime UID, image, native policy,
  credentials, endpoints, or namespaces;
- an AgentInstance has at most one active AgentRuntime;
- AgentRuntime replacement does not change AgentInstance identity; and
- cleanup always targets the exact runtime UID.

Direct client submission of arbitrary AgentInstance CRDs is not a user-facing
creation path. The API performs authentication, canonical identity resolution, and
admission before desired state is written.

## AgentSession contract

`AgentSession` is a durable Steward API object backed by an append-only journal. It is
not a CRD and does not carry authority.

- A session belongs to an AgentInstance, never directly to a runtime UID.
- Runtime replacement does not destroy the session.
- WebSocket, SSE, TUI, web, and connector subscriptions are transient attachments.
- Session inputs use idempotency keys and durable monotonic sequencing.
- Creation, input, Task linkage, control intent, expiry, and closure are journaled.
- Live output is projected through SessionRelay with bounded buffering and cursors.
- Slow or disconnected subscribers never block agent execution.
- Subscribers are reauthorized during long-lived streams.
- Session events contain no provider credentials, HOP-1 tokens, SVIDs, or inference
  keys.

A GitHub Actions Task has no AgentSession. A Burble, TUI, or web conversation maps to
one AgentSession containing zero or more Tasks.

## Conversational Task boundary

One independently executable objective creates one Task. A Task may contain multiple
model calls, tool calls, responses, and clarification turns.

- A new objective creates a new Task.
- Clarification may continue the same Task only while it is explicitly parked waiting
  for input.
- Supplemental input is durably receipted and cannot silently change the original
  input or widen authority.
- Cancel, approve, reject, suspend, resume, and terminate are typed control intents,
  not ordinary chat Tasks.
- Subscription and history reads create no Tasks.
- Session context supplied to a Task is bounded and explicitly snapshotted.

The accepted baseline has no conversation-long parent Task and does not create a Task
for passive or non-effectful UI activity.

## Persistent runtime and isolation

A long-running AgentInstance maintains one exclusive persistent AgentRuntime and agent
service across Tasks. Runtime reuse is allowed but never bypasses Task admission.

- A runtime belongs to exactly one AgentInstance and canonical owner.
- Sandboxes, workload identities, workspaces, provider attachments, and inference
  bindings are not shared between AgentInstances.
- Per-Task input and output are isolated.
- Persistent state is retained only through an explicit AgentInstance-owned storage
  contract.
- Process memory, runtime scratch, open connections, and runtime credentials belong to
  the exact runtime UID and are disposable.
- Runtime adoption by reusable name is forbidden.

The runtime holds a standing authority ceiling:

```text
runtime authority =
    owner Envelope
  intersect admitted AgentInstance request
  intersect deployment-owned native binding
```

Every TaskDefinition must fit inside that ceiling before dispatch. A Task cannot widen
the resident runtime. Documentation must not claim task-specific sandbox attenuation
unless an enforcement mechanism and conformance test prove it.

The existing M1 one-shot Task lane remains separate from the long-running
AgentInstance lane.

## Concurrency

An AgentInstance may have multiple authorized sessions and concurrent observers, but
the initial execution contract is single-lane:

- at most one Task executes per AgentInstance;
- accepted Tasks enter a bounded durable per-instance queue;
- Steward assigns ordering;
- control operations take precedence over normal queued work;
- duplicate submissions reuse their idempotency result;
- an executing clarification wait retains the lane only for a bounded interactive
  deadline; and
- parallel Task execution requires a future explicit capability and isolation proof.

Cross-user collaborative sessions are outside the initial contract.

## Identity

Every initially supported AgentInstance is a non-transferable standing delegation for
one canonical user. Its runtime uses a fixed Steward-owned delegated-service identity
for that owner.

Conceptually:

```text
iss       = configured Steward Mint issuer
sub       = canonical owner user ID
acting_as = service_for_user
service   = fixed Steward managed-agent service class
azp       = current runtime workload identity
```

The browser, administrator, Agent artifact, and `agentic-ops` cannot select the service
identity. Its exact registered string is frozen only with the later Mint/runtime
contract.

For administrator-created instances, creator and owner are distinct audit facts. The
administrator does not become the runtime subject, credential owner, acting user, or
automatic session participant. Email, Slack user ID, and other channel identities are
never ownership or credential join keys.

Team-owned, transferable, shared, and pure-service AgentInstances require separate
future contracts.

## Suspension, scale-down, and recovery

Suspension is a governance state:

- reject new Tasks;
- interrupt or cancel active work according to explicit terminal semantics;
- revoke active authority;
- finalize the exact runtime UID; and
- require an authorized resume operation.

Resume revalidates current Envelope, revocation, catalog, and deployment bindings and
provisions a fresh runtime UID. The initial design has no opaque process or sandbox
checkpoint and never silently replays an interrupted Task.

Automatic idle scale-down is different:

- the AgentInstance remains admitted and available;
- no runtime is active while cold;
- the next admitted Task may automatically provision a fresh runtime; and
- no manual resume is required.

Session journals and explicitly declared AgentInstance-owned durable state may survive
scale-down, replacement, and suspension. Process memory, runtime scratch, and
credentials do not.

## Artifact publication and AgentInstance upgrades

Publication, admission, and activation are distinct:

1. `agentic-ops` publishes an immutable artifact through the normal create-only
   catalog path.
2. Publication makes the artifact available but changes no running AgentInstance.
3. An owner or administrator proposes an immutable AgentInstance revision.
4. Steward resolves the complete closure and computes before/after authority deltas.
5. In-envelope changes may be admitted after owner confirmation; excessive changes
   park unchanged for approval.
6. Activation drains or cancels old work as required, provisions a replacement, and
   finalizes the old exact runtime UID.

There is no `latest` resolution. Queued and running Tasks remain bound to the revision
under which they were admitted and are never reinterpreted. Rollback is another
admitted revision referencing an older immutable artifact, not history mutation.

A successful Task may produce a candidate artifact change, but success is evidence,
not publication or approval.

## Durable execution implementation

The accepted baseline uses Steward's existing Postgres journal and Kubernetes
reconciliation. It introduces no external workflow engine. The required behavior is:

- persist transitions before acknowledging side effects;
- idempotent retries;
- no reinterpretation of persisted bindings;
- recovery after apiserver or controller restart;
- explicit cancellation and deadlines;
- durable journal ordering; and
- recoverable exact-runtime finalization.

An external durable engine may be reconsidered only with a separately accepted
TaskGraph or orchestration requirement.

## Burble and TUI boundary

Burble becomes a connector over AgentSession and SessionRelay rather than a runtime
control plane:

```text
Slack input
  -> Burble resolves canonical Steward user
  -> Burble opens or reuses AgentSession
  -> Burble submits Task or typed control intent
  -> Steward admits and dispatches
  -> SessionRelay streams typed events
  -> Burble renders Slack output
```

Burble does not own sandbox provisioning, runtime identity, policy, credential
attachment, Task admission, reaping, or cleanup. TUI and web surfaces use the same
session boundary. Channel adapters carry authenticated intent and presentation; they
do not make governance decisions.

Burble's current automatic idle reaping maps to Steward idle scale-down, not governance
suspension. Scheduled-job definitions may remain Burble-owned initially, but every
effectful timer firing submits an admitted Steward Task rather than directly invoking
a runtime.

## Repository and system ownership

### `agentic-ops` owns

- portable Agent and role packages;
- prompts and instruction-only skills;
- TaskDefinition source;
- dependency locks and declared portable requirements;
- independent artifact tests; and
- authored source and promotion metadata.

It does not own or select users, Envelopes, grants, credentials, provider connections,
runtime identity, deployment endpoints, namespaces, RuntimeClasses, native OpenShell
profiles, images, or mutable Task/AgentInstance state.

### Steward owns

- publication witnesses and immutable resolution;
- canonical identity mapping;
- Envelopes, grants, admission, and revocation;
- AgentInstance, AgentRuntime, Task, and AgentSession lifecycle;
- credential readiness and runtime bindings;
- journals, evidence, finalization, and cleanup; and
- the APIs and schemas that enforce these boundaries.

### Deployment operators own

- concrete images and provenance/trust selection;
- native runtime bindings;
- RuntimeClasses, namespaces, network policy, and infrastructure endpoints; and
- environment-specific capacity and storage configuration.

### Connectors own

- external-channel identity resolution into canonical Steward identity;
- inbound intent normalization;
- surface-specific narration and rendering; and
- NotificationSink and SessionRelay adaptation.

Connectors never own admission, credentials, runtime lifecycle, or approval decisions.

## Required documentation changes

1. Add `docs/README.md` with a document map and explicit status labels.
2. Add a `docs/v2/` index describing accepted semantics and remaining implementation
   contracts.
3. Reconcile or replace `docs/workflow-and-task-spec.md`; retain a short historical
   redirect if content moves.
4. Mark the multi-step Workflow object as unadopted historical exploration and use
   `TaskGraph` only as a provisional future term.
5. Add status banners and corrected cross-links to:
   - `docs/m1-delivery-plan.md`;
   - `docs/solution-overview.md`;
   - `docs/steward-ai-workflows-fit.md`;
   - `docs/data-plane-spec.md`; and
   - `docs/roadmap/steward-roadmap.md`.
6. Remove claims that unimplemented capabilities already exist.
7. Consolidate deferred implementation contracts in one document rather than
   maintaining conflicting decision tables.

## Deferred implementation contracts

The current set is maintained only in the
[consolidated deferred-contract register](deferred-implementation-contracts.md). Those
items do not reopen an existing M1 or public contract and require separate design and
acceptance.

## Non-goals

- No production code, migration, CRD, generated client, Helm, or deployment change.
- No modification of `docs/contracts/m1/v1/**`.
- No implementation of AgentInstance, AgentSession, TaskGraph, TUI, or Burble changes.
- No MCP-GW, LiteLLM, Mint, OpenShell, or provider-connection change.
- No GitOps or environment-specific work.
- No Linear changes.
- No repository-rule changes in this documentation ticket.

## Acceptance criteria

- A new reader can identify the authoritative M1 contract in under one minute.
- Every reviewed architecture document has a visible status.
- `Agent`, `TaskDefinition`, `Task`, GitHub Actions workflow, `AgentInstance`,
  `AgentRuntime`, and `AgentSession` have one consistent meaning.
- M1 behavior and accepted post-M1 semantics are never presented as one wire contract.
- GitHub Actions, Burble, TUI, scheduled work, idle scale-down, suspension, runtime
  replacement, and immutable upgrades are explained consistently.
- Steward, `agentic-ops`, operator, and connector responsibilities do not overlap.
- The frozen M1 contract tree is byte-for-byte unchanged.
- Internal links resolve and duplicate/superseded decision registers are removed.
- The documentation-only gate is green: `git diff --check origin/main...HEAD`,
  `cargo xtask check-neutrality`, and `cargo xtask check-secrets`.
