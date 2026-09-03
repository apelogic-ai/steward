# Steward post-M1 architecture baseline

Status: accepted post-M1 semantics; implementation and wire contracts pending

This document defines the accepted vocabulary and behavioral boundaries for Steward's
post-M1 long-running-agent lane. It does not claim that the resources, APIs, or user
experiences below are implemented, and it does not freeze their wire representation.
The [frozen `steward.m1/v1` contract](../contracts/m1/v1/README.md) remains the sole
normative M1 contract.

See the [documentation index](../README.md) for authority order and the
[deferred-contract register](deferred-implementation-contracts.md) for decisions that
still require separate design and acceptance.

## Canonical terminology

| Term | Accepted meaning |
|---|---|
| `Agent` | Immutable portable catalog artifact describing an agent interface. |
| `TaskDefinition` | Immutable definition of one governed execution atom. |
| `Task` | One admitted invocation of a `TaskDefinition`. |
| GitHub Actions workflow | A trigger and transport that may invoke one Steward `Task`. |
| M1 `workflow` field | Frozen historical field name whose value resolves a `TaskDefinition`. |
| `AgentInstance` | Persistent governed standing delegation for one canonical user. |
| `AgentRuntime` | Replaceable execution incarnation of an `AgentInstance`, identified by exact runtime UID. |
| `AgentSession` | Durable interactive conversation or attachment associated with an `AgentInstance`. |
| `SessionRelay` | Typed streaming boundary consumed by TUI, web, or connector adapters. |
| role implementation | Portable process or image implementing an agent or execution role. |

Steward has no accepted domain object named `Workflow`. The former multi-step Workflow
proposal is [historical](../workflow-and-task-spec.md). If Steward later needs to own
dependencies, per-node authority, branching, durable inter-node pause/resume, or
graph-level evidence, that work requires a separately accepted `TaskGraph`-like
contract. `TaskGraph` is provisional terminology, not a resource or API.

## Two execution lanes, one governed Task atom

`TaskDefinition` is the portable governed atom and `Task` is one admitted invocation.
Both execution lanes must use the common admission and Task application service.

```text
GitHub Actions workflow                    AgentSession input
        -> steward-run                            |
        -> M1 workflow coordinate                 v
        -> TaskDefinition               AgentInstance binding
                     \                    /
                      -> admitted Task <-
```

A normal GitHub Actions invocation creates exactly one Task and no AgentSession. Its
frozen M1 lifecycle remains unchanged. A long-running AgentInstance independently
executes admitted Tasks; its sessions organize conversation and observation but carry
no authority. No accepted baseline includes a multi-Task graph scheduler.

## AgentInstance and AgentRuntime

`AgentInstance` is the persistent desired-state concept and will be CRD-backed. Desired
state is written only through Steward admission, after server-side authentication and
canonical identity resolution. Controller-authored CRD status is authoritative for
current operational phase. Postgres stores history, idempotency, journal events, and UI
projections, not authoritative current phase.

Direct client submission of an arbitrary AgentInstance CRD is not a user-facing
creation path.

The eventual wire schema is deferred. These semantic properties are accepted:

- the canonical owner is server-authored;
- Agent, TaskDefinition, Envelope, and deployment bindings are immutable or
  revision-pinned;
- clients cannot choose identity, runtime UID, image, native policy, credentials,
  endpoints, namespaces, or execution mode;
- an AgentInstance has at most one active AgentRuntime;
- runtime replacement preserves AgentInstance identity; and
- cleanup and adoption decisions use the exact runtime UID, never a reusable name.

A long-running AgentInstance maintains one exclusive persistent AgentRuntime and agent
service across Tasks, but every Task is separately admitted. The runtime's standing
authority ceiling is:

```text
owner Envelope
intersect admitted AgentInstance request
intersect deployment-owned native binding
```

Every TaskDefinition must fit within that ceiling before dispatch. No document may
claim per-Task sandbox attenuation until an enforcement mechanism and conformance test
prove it. The disposable M1 Task lane remains separate from this resident-runtime lane.

## AgentSession and conversational Tasks

`AgentSession` is a durable Steward API object backed by an append-only journal. It is
not a CRD and grants no authority.

- A session belongs to an AgentInstance, never directly to a runtime UID.
- Runtime replacement, scale-down, or suspension does not destroy the journal.
- WebSocket, SSE, TUI, web, and connector subscriptions are transient attachments.
- Inputs use idempotency keys and durable monotonic sequencing.
- Creation, input, Task linkage, control intent, expiry, and closure are journaled.
- SessionRelay provides bounded buffering and cursors; slow or disconnected
  subscribers never block execution.
- Long-lived subscriptions are reauthorized.
- Events contain no provider credentials, HOP-1 tokens, SVIDs, or inference keys.

One independently executable objective creates one Task, and a new objective creates a
new Task. A Task may include multiple model calls, tool calls, responses, and
clarification turns. Clarification remains in the same Task only while it is explicitly
parked for bounded input. Supplemental input is durably receipted and cannot silently
replace the original input or widen authority.

Cancel, approve, reject, suspend, resume, and terminate are typed control intents, not
ordinary chat Tasks. Subscription and history reads create no Tasks. Session context
provided to a Task is bounded and explicitly snapshotted. The baseline has no
conversation-long parent Task and creates no Task for passive UI activity.

A GitHub Actions Task has no AgentSession. A connector, TUI, or web conversation maps
to one AgentSession containing zero or more Tasks.

## Concurrency and isolation

An AgentInstance may have multiple authorized sessions and concurrent observers, but
the initial execution contract is single-lane:

- at most one Task executes per AgentInstance;
- accepted Tasks enter a bounded durable per-instance queue in Steward-assigned order;
- control operations take precedence over normal queued work;
- duplicate submissions reuse their idempotency result;
- clarification holds the lane only for a bounded interactive deadline; and
- parallel Task execution requires a future capability and isolation proof.

A runtime belongs to exactly one AgentInstance and canonical owner. AgentInstances do
not share sandboxes, workload identities, workspaces, provider attachments, inference
bindings, or runtime credentials. Per-Task inputs and outputs are isolated. Durable
state survives only through an explicit AgentInstance-owned storage contract; process
memory, scratch, connections, and credentials belong to the exact runtime UID and are
disposable. Cross-user collaboration is deferred.

## Identity

Every initially supported AgentInstance is a non-transferable standing delegation for
one canonical user. Its runtime uses a fixed Steward-owned delegated-service identity
for that owner. Conceptually:

```text
iss       = configured Steward Mint issuer
sub       = canonical owner user ID
acting_as = service_for_user
service   = fixed Steward managed-agent service class
azp       = current runtime workload identity
```

The exact service string is part of the deferred Mint/runtime contract. Browsers,
administrators, Agent artifacts, and `agentic-ops` cannot select it. For an
administrator-created instance, creator and owner are separate audit facts; the
administrator does not become the runtime subject, credential owner, acting user, or
automatic session participant. Channel identifiers such as email or Slack user ID are
never ownership or credential join keys.

Team-owned, transferable, shared, and pure-service AgentInstances need future
contracts.

## Suspension, scale-down, recovery, and upgrades

Suspension is a governance transition: reject new Tasks, apply explicit terminal
semantics to active work, revoke active authority, finalize the exact runtime UID, and
require an authorized resume. Resume revalidates current Envelope, revocation, catalog,
and deployment bindings and provisions a fresh runtime UID. There is no opaque process
checkpoint and no silent replay of interrupted work.

Idle scale-down is not suspension. The AgentInstance remains admitted and available,
has no active runtime while cold, and may provision a fresh runtime automatically for
the next admitted Task. No manual resume is required. Session journals and explicitly
declared durable state may survive; runtime-local state and credentials do not.

Publication, admission, and activation are distinct. Artifact publication is
create-only and changes no running instance. An owner or administrator proposes an
immutable AgentInstance revision; Steward resolves its full closure and computes
authority deltas. In-envelope changes may be admitted after owner confirmation;
excessive changes park the unchanged instance for approval. Activation drains or
cancels old work as required, provisions a replacement, and finalizes the old exact
runtime UID.

There is no `latest` resolution. Queued and running Tasks retain the immutable revision
under which they were admitted. Rollback is another admitted revision that references
an older immutable artifact. Task success may provide evidence for a candidate artifact
change, but it is never publication or approval.

## Durability model

The accepted baseline extends Steward's Postgres journal and Kubernetes reconciliation;
it does not introduce an external workflow engine. Implementation must persist
transitions before acknowledging side effects, make retries idempotent, retain immutable
bindings, recover after apiserver or controller restarts, support explicit cancellation
and deadlines, preserve journal order, and recover exact-runtime finalization.

An external durable engine is reconsidered only if a separately accepted TaskGraph or
orchestration requirement justifies it.

## Product and repository boundaries

| Owner | Responsibilities | Explicit exclusions |
|---|---|---|
| `agentic-ops` | Portable Agent and role packages, prompts, instruction-only skills, TaskDefinition source, dependency locks, portable requirements, independent tests, source and promotion metadata | Users, Envelopes, grants, credentials, provider connections, runtime identity, deployment endpoints, namespaces, RuntimeClasses, native profiles, concrete deployment images, and mutable Task or AgentInstance state |
| Steward | Publication witnesses and immutable resolution; canonical identity; Envelopes, grants, admission, revocation; AgentInstance, AgentRuntime, Task, and AgentSession lifecycle; binding readiness; journals, evidence, finalization, cleanup; enforcing APIs and schemas | Portable role implementation ownership and channel-specific presentation |
| Deployment operator | Concrete images and provenance selection, native runtime bindings, RuntimeClasses, namespaces, network policy, infrastructure endpoints, capacity, and storage configuration | User identity, admission, or application lifecycle decisions |
| Connector | Canonical-user resolution for its channel, inbound intent normalization, narration and rendering, NotificationSink and SessionRelay adaptation | Admission, credentials, runtime lifecycle, cleanup, and approval decisions |

A connector such as Burble opens or reuses an AgentSession, submits a Task or typed
control intent, consumes SessionRelay events, and renders them for its channel. TUI and
web clients use the same session boundary. Scheduled definitions may remain
connector-owned initially, but each effectful firing submits an admitted Task. Existing
automatic idle reaping maps to idle scale-down, not governance suspension.
