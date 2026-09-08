# Durable Task runtime orchestration

Status: accepted and implemented on the v2 common-core branch

This document defines the target internal architecture for applying one immutable
Steward `Task` across Postgres, Kubernetes, approval, runtime-provider, and execution
boundaries. It is an implementation decision, not a public wire contract and not a
claim about the public wire contract.

The frozen [`steward.m1/v1` contract](contracts/m1/v1/README.md) remains authoritative
for M1 request, response, phase, evidence, and finalization behavior. The
[post-M1 architecture baseline](v2/README.md) remains authoritative for accepted
`TaskDefinition`, `Task`, `AgentInstance`, `AgentRuntime`, and `AgentSession` semantics.
This design supplies the durable orchestration mechanism required by both execution
lanes without introducing a Steward `Workflow`, public Run object, or external workflow
engine.

## Decision

Steward will implement Task application as a durable, single-owner reconciliation
state machine.

- The API server authenticates, validates, resolves immutable inputs, and records Task
  and control intent in Postgres. It performs no Kubernetes, approval-channel, runtime,
  model, tool, credential, or execution side effect.
- One logical Task orchestrator owns admission materialization, inert runtime creation,
  exact UID observation, approval progression, authority activation, execution,
  cancellation, and cleanup.
- Every external effect is preceded by durable intent and followed by a compare-and-set
  observation. A failed or ambiguous call is resolved by observing durable and external
  state; it never triggers an immediate compensating delete or an HTTP retry side
  effect.
- Every Task-owned runtime begins inert. Steward persists its exact Kubernetes UID
  before enabling model, tool, credential, or external-network authority.
- Task execution is never silently replayed. An adapter must support an idempotent,
  observable execution attempt, or an ambiguous start/result becomes a terminal
  `execution_outcome_unknown` finding and proceeds to cleanup.
- Finalization is complete only after every Task-owned projection is confirmed absent.

This replaces recovery based on nullable fields, reusable names, reconstructed object
shape, or submission retries.

## Implementation map

The architecture is implemented by these internal boundaries:

- append-only migration `0028_durable_task_runtime_orchestration.sql` creates the
  operation, execution-attempt, external-effect outbox, and orchestration-journal
  records and fences old lifecycle writers;
- `steward-store` owns locked, generation-checked transitions and revalidates current
  authority in the activation and execution-start transactions;
- `steward-apiserver` records immutable Task intent and monotonic commands only;
- the Task reconciler in `steward-controller` owns inert creation, exact UID
  observation, admission materialization, activation, execution, and cleanup;
- the controller's approval dispatcher drains durable outbox records through the
  `DecisionChannel` port using the approval UUID as the stable delivery identity;
- `SandboxTaskRuntime` exposes attempt-scoped start, observation, and cancellation;
  the OpenShell adapter persists attempt markers and fails closed on an ambiguous
  outcome instead of replaying it; and
- `TaskExecutionAdapter` keeps agent-specific command generation out of core. The
  Codex implementation lives in `adapters/codex`, while deployment-selected images,
  executables, versions, profiles, and network endpoints remain configuration.

The focused Postgres and fake-Kubernetes fault suite covers concurrent reconcilers,
ambiguous create and execution results, UID replacement, authority revision and
revocation races, state monotonicity, and exact-UID cleanup. The pinned full-stack lane
remains the conformance proof for the real external components.

## Scope

This design covers:

- disposable Task-owned `AgentRuntime` creation and teardown;
- the exact-identity and cleanup boundary for adopted or resident runtimes, while their
  dispatch wire protocol remains deferred;
- baseline admission and approval-backed admission;
- exact runtime-UID-bound grants;
- immutable execution bindings;
- execution request, result, cancellation, and ambiguity;
- approval-channel filing as a recoverable external effect;
- controller concurrency, restart, and uncertain database or network outcomes; and
- migration from the existing `task_submissions` lifecycle.

It does not define:

- a multi-Task graph or Steward `Workflow` object;
- a public Run or execution-attempt API;
- an opaque process checkpoint or interrupted-command replay;
- a replacement for `AgentRuntime` CRD status as the current runtime-phase authority;
- the deferred resident-agent dispatch wire protocol; or
- new M1 fields or phase values.

## Why the current shape is insufficient

Task application crosses systems that cannot share one transaction:

```text
Postgres                  Kubernetes               Approval / execution systems
--------                  ----------               ----------------------------
reserve Task
runtime UID unknown
                          create AgentRuntime
                          assign immutable UID
                                                   file or observe approval
bind UID
                                                   start command and collect result
                          finalize runtime
record finalization
```

A deterministic Kubernetes name makes a create retry addressable, but it does not make
the name an identity. Kubernetes names can be deleted and reused; cleanup and authority
must join on the exact UID.

The existing combination of Task phase, nullable `runtime_uid`, `execute_requested`,
`finalize_requested`, and `finalized` does not say which external effects were intended,
attempted, observed, or made ambiguous. In particular, an unbound Task can mean that no
runtime was attempted, an inert approval placeholder exists, a create succeeded but its
UID was not committed, or a binding commit succeeded but its result was lost. Recovery
then has to infer history from a reusable name and current object shape.

The architectural defect is not that a particular transition lacks another condition.
It is that external-effect progress is not a first-class durable fact.

## Non-negotiable invariants

The implementation must make these invariants structural:

1. **One Task intent.** A submitter-scoped idempotency key resolves to one immutable
   Task intent. A changed immutable input conflicts. A retry only reads that intent and
   never performs orchestration.
2. **One Task lifecycle writer.** Only the Task orchestrator advances orchestration
   state or causes Task-owned external effects. API handlers append commands; they do
   not resume work inline.
3. **Intent before effect.** No external mutation occurs without a committed operation
   state authorizing exactly that mutation.
4. **Observation after effect.** External success is not assumed from a response. The
   reconciler observes the object or result and commits that observation with a
   generation-checked transition.
5. **Inert before identified.** A newly created Task-owned runtime has no model, tool,
   credential, or external-network authority until its exact UID is durable.
6. **UID before grant.** Approval and grants for a provisioned runtime are created only
   after the exact runtime UID is durable. Names never authorize, bind, or select.
7. **UID immutability.** Once a Task observes a runtime UID, that UID never changes. A
   missing or replaced runtime fails the Task; the same Task is never rebound to a new
   incarnation.
8. **Authority before activation and execution.** Effective authority is checked in the
   same locked transaction that authorizes activation or claims execution.
9. **No backward transitions.** Reconciliation state and execution-attempt state move
   through explicit allowed edges. Idempotent repetition returns the current state and
   never rewrites an earlier phase.
10. **No cleanup from ambiguity.** A timeout, connection loss, or database error alone
    never authorizes deletion. Cleanup requires durable cleanup intent and exact
    ownership evidence.
11. **Final means absent.** A Task-owned runtime is finalized only after the exact UID
    and every provider, credential, network, and sandbox projection are confirmed
    absent.
12. **No silent execution replay.** The same Task execution attempt is started at most
    once logically. If its outcome cannot be proven, Steward fails closed and requires
    a new Task for a deliberate rerun.
13. **Desired sets are reconciled as sets.** A provider or authority projection omitted
    from desired state is explicitly detached; reconciliation is not attach-only.
14. **Leases are optimization, not correctness.** Concurrent reconcilers remain safe if
    a lease expires or two workers observe the same state.
15. **Deployment contracts are data.** Core orchestration never invents a default agent,
    executable, image, endpoint, provider profile, version, or native-policy generation.
    It validates and persists the exact deployment binding selected before reservation.

## Ownership boundaries

| Component | May write | Must not do |
|---|---|---|
| API server | Immutable Task intent; immutable input receipt linkage; monotonic execute, cancel, and finalize commands | Create, replace, or delete runtimes; file approvals; activate authority; run commands; infer recovery from an HTTP retry |
| Task orchestrator | Internal orchestration transitions; approval materialization; `AgentRuntime` desired state; exact UID observation; execution-attempt state; cleanup completion | Accept caller-owned execution fields; mutate immutable Task intent; treat a reusable name as identity |
| AgentRuntime controller | CRD status and adapter projections for the exact CR UID | Decide Task admission; select a different Task binding; write Task business intent |
| Admission library | Pure deterministic evaluation of an immutable candidate against an explicit authority snapshot | Perform I/O; create grants; alter an Envelope |
| Approval adapter / outbox dispatcher | Deliver one durable approval notification and observe its external reference | Create a second approval; decide authority outside the approval/grant store |
| Runtime adapter | Reconcile the requested provider set and execute/observe a named attempt | Invent profiles, credentials, or bindings; retain omitted providers |
| Postgres | Immutable intent, command journal, orchestration state, authority history, exact observations, and audit events | Claim authoritative live runtime phase in place of CRD status |

The Task orchestrator and AgentRuntime controller may run in the same binary, but they
remain separate logical reconcilers. The Task orchestrator owns the Task lifecycle and
the desired `AgentRuntime` CR. The AgentRuntime controller owns that CR's status and its
OpenShell, model, tool, network, and credential projections.

## Separate business, orchestration, and runtime state

Three kinds of state must not be collapsed:

| State | Examples | Authority |
|---|---|---|
| Task business state | submitted, parked, queued, running, succeeded, failed, cancelled | Postgres Task aggregate, projected through the active public contract |
| Orchestration progress | intent recorded, runtime create pending, UID observed, approval pending, activation pending, active, cleanup pending, finalized | Postgres orchestration record, single-writer Task orchestrator |
| Runtime operational state | pending, provisioning, running, suspended, terminating | `AgentRuntime.status`, single-writer AgentRuntime controller |

Task phase describes what the invocation means to its client. Orchestration state says
which recoverable external effect Steward must reconcile next. Runtime status describes
the current observed data-plane incarnation. No one field substitutes for another.

`runtimeUid` remains a nullable diagnostic projection in public M1 evidence. Internally,
once observed, it is the immutable identity for all later authority and cleanup.

Public phase projection remains monotonic and contract-owned:

- a baseline Task is `submitted` while orchestration prepares its runtime;
- an excessive Task is `parked` from its durable submission decision through approval;
- an execution command may be recorded while parked, but phase becomes `queued` only
  after activation authority and runtime readiness are both durable;
- `running` begins only after the exact execution attempt is acknowledged; and
- a terminal business phase does not imply finalization or permit orchestration state
  to move backward.

## Durable orchestration states

The internal Task-owned runtime state machine is:

```text
intent_recorded
      |
      v
runtime_create_pending
      |
      v
runtime_observed ----------------------+
      |                                |
      | baseline authority             | excessive candidate
      v                                v
activation_pending              approval_pending
      ^                                |
      | exact active grant             | reject, expire, revoke,
      +--------------------------------+ or envelope superseded
      |                                |
      v                                v
active                          cleanup_pending
      |                                |
      +---- terminal/finalize ---------+
                                       |
                                       v
                                   finalized
```

Cancellation, authority loss, or a permanent invariant failure may move any
non-finalized state to `cleanup_pending`. A Task cancelled before any runtime effect may
move from `intent_recorded` directly to `finalized` after final evidence is committed.

The earlier shorthand state `bound` is intentionally split. `runtime_observed` means
the exact UID association is durable while the runtime remains inert. `active` means
authority was separately validated and the exact active desired state is ready. UID
association never implies authority activation.

`failed`, `rejected`, and `cancelled` are Task outcomes, not provisioning states. A Task
may already have a terminal business phase while orchestration remains
`cleanup_pending`. This prevents “failed” from being mistaken for “safe to forget.”

### State contracts

| State | Durable fact | Permitted next effect |
|---|---|---|
| `intent_recorded` | Immutable Task, candidate, authority snapshot, execution binding, active and inert manifest digests, and operation ID exist; no runtime effect is authorized | Commit `runtime_create_pending`, unless cancellation can finalize without a runtime |
| `runtime_create_pending` | Creation of exactly the inert manifest is authorized | Create or get the deterministic inert runtime and observe its UID |
| `runtime_observed` | Exact UID and initial resource version are durable; runtime is still inert | Atomically choose current baseline activation, materialize one approval, or terminalize to cleanup |
| `approval_pending` | One append-only decision and approval are bound to Task UID, runtime UID, candidate digest, and Envelope revision | Observe pending, approved, rejected, expired, revoked, or superseded authority |
| `activation_pending` | A locked authority decision authorizes the exact candidate for the exact runtime UID | Apply the active manifest to that UID and wait for readiness evidence |
| `active` | Exact active manifest is observed and required provider bindings are ready | Accept/claim execution or process cancellation/finalization |
| `cleanup_pending` | No future activation or execution is allowed; teardown of owned projections is required | Revoke/detach desired authority, delete exact runtime UID, and observe absence |
| `finalized` | Owned projections are confirmed absent and final evidence is durable | No effect; reads only |

Every transition increments an orchestration generation. State-specific database
constraints reject missing or contradictory fields. A transition that finds a newer
generation returns `Superseded(current)` rather than a generic invalid-transition error.

## Inert runtime contract

Every provisioned Task runtime starts from a server-authored inert manifest. The inert
manifest:

- retains canonical owner, service principal, Task UID, operation ID, execution-binding
  digest, namespace, and deterministic name;
- contains no models or tools;
- requests no provider profiles;
- has zero spend authority;
- has no task command in progress;
- permits no external network path except infrastructure required to report status;
- carries a bounded bootstrap TTL; and
- carries the inert manifest digest in a Steward-owned annotation.

The exact inert manifest is derived before reservation and its canonical digest is
stored with Task intent. Canonicalization excludes Kubernetes-assigned fields,
`status`, resource version, and non-Steward metadata. The active manifest digest covers
the exact candidate specification and immutable execution binding.

Creating the inert CR must not itself mint model/tool credentials or attach their
providers. This is a security property requiring a negative conformance test. The
AgentRuntime controller attaches providers only after desired state changes to the
active manifest, and explicitly detaches every Steward-managed provider omitted from
desired state.

## Execution-binding boundary

The orchestration state machine is agent-neutral. Before reservation, the deployment
catalog resolves an opaque logical agent reference to an exact binding containing the
adapter contract, digest-pinned image, executable and version probe, and required
provider-profile IDs and digests. The complete validated binding is stored in immutable
Task intent and covered by the active manifest digest.

Core orchestration resolves a registered versioned adapter contract. Command and agent
configuration rendering belong to that adapter. Core Task code must not contain a
Codex command, `CODEX_HOME`, a LiteLLM cluster URL, npm package path, provider-profile
name, or fallback agent/version. Supporting a new deployment instance is catalog data;
supporting a new execution protocol is an explicit new adapter contract.

An empty or missing catalog advertises no logical agents and rejects new Task
reservation before intent is written. Historical Tasks continue only from their
persisted binding; reconciliation never consults the current catalog or substitutes a
new binding.

## Kubernetes identity and idempotency

The deterministic runtime name is derived from the Task orchestration operation ID. It
is only a rendezvous key. Every server-authored runtime carries these immutable
Steward-owned identities:

```text
agents.apelogic.ai/task-uid=<task UUID>
agents.apelogic.ai/orchestration-id=<operation UUID>
agents.apelogic.ai/manifest-digest=<inert or active digest>
agents.apelogic.ai/runtime-mode=inert|active
```

On create:

1. The reconciler commits `runtime_create_pending` before calling Kubernetes.
2. It creates the exact inert object with the deterministic name.
3. On success, timeout, disconnect, or `AlreadyExists`, it reads that name.
4. It accepts the object only when Task UID, operation ID, controlled desired fields,
   and inert manifest digest all match.
5. It commits the observed UID using a generation compare-and-set.

An object with the same name but a different operation ID, Task UID, or controlled
manifest is an ownership collision. Steward records a safe failure and never adopts,
mutates, or deletes it.

Once a UID is observed, all get, patch, activation, and delete operations verify that
exact UID. If the name now resolves to another UID, the original runtime is treated as
gone and the Task proceeds to failure/cleanup. The replacement is unrelated and is not
touched.

### Adopted and resident runtime boundary

An adopted or resident Task does not authorize `runtime_create_pending`. Its immutable
deployment/instance binding supplies a server-resolved runtime identity, and the Task
orchestrator independently observes the exact UID and matching owner before recording
`runtime_observed`. A public M1 caller never supplies or selects this UID.

The Task may proceed only if the resident runtime's immutable binding, current standing
authority ceiling, canonical owner, and operational readiness all match. Task-specific
approval, if supported by the eventual resident dispatch contract, still binds to that
exact UID. Task finalization cancels its execution attempt and removes its Task-owned
authority projections, but skips runtime deletion. Nothing in this document resolves
the deferred resident dispatch wire protocol or permits a Task to mutate a shared
runtime envelope.

## Admission and authority

### Submission-time work

Before recording Task intent, the API server performs the contract-required
authentication, request validation, immutable artifact resolution, identity binding,
input ownership checks, and pure admission evaluation. Invalid or unauthorized input
creates no Task.

The reservation store operation locks the relevant Envelope scope and revalidates the
resolved revision, digest, and pure decision inside the transaction. Resolution outside
that transaction cannot commit a stale authority snapshot.

The reservation transaction stores:

- the complete immutable candidate and its digest;
- TaskDefinition/legacy M1 workflow coordinate and digest;
- Envelope identity, revision, digest, and evaluated decision;
- canonical principal and owner bindings;
- immutable deployment execution binding;
- active and inert runtime manifest digests;
- deterministic orchestration ID, namespace, and runtime name; and
- initial Task phase plus `intent_recorded` orchestration state.

For an excessive candidate, the transaction records the immutable rejection deltas and
projects the Task as parked. It does not create a runtime-bound admission decision or
contact the decision channel because the Kubernetes UID does not exist yet.

### Runtime-observed authority decision

After the inert UID is durable, one locked transaction chooses exactly one path:

1. If the latest Envelope admits the unchanged candidate, record the exact Envelope
   revision and digest as baseline activation authority and enter
   `activation_pending`.
2. Otherwise, if the Task's original excessive decision is still based on the current
   Envelope revision and digest, create exactly one append-only admission decision and
   approval bound to the exact runtime UID, enqueue its decision-channel outbox entry,
   and enter `approval_pending`.
3. Otherwise, atomically record authority withdrawal, set the contract-appropriate
   terminal Task outcome/failure code, and enter `cleanup_pending`.

This permits a harmless Envelope revision bump to authorize an unchanged baseline
candidate when the latest Envelope still admits it. It never carries an old approval or
grant across an Envelope revision. If the latest Envelope now admits a previously
excessive candidate before an approval is materialized, activation may use that newly
recorded baseline authority; it does not manufacture or reuse a grant.

### Approval-backed activation

Approval history alone never authorizes activation. The transition from
`approval_pending` to `activation_pending` locks the Task, approval, grants, revocations,
and current Envelope scope and requires:

- exactly one admission decision for the Task;
- the same Task UID, runtime UID, operation ID, namespace, and name;
- the same candidate, deltas, and candidate digest;
- the same still-current Envelope revision and digest;
- the exact expected number and dimensions of grants;
- every grant bound to the exact runtime UID and Envelope revision;
- every grant unexpired and unrevoked; and
- no cancellation or finalization command.

If any requirement fails, the same transaction terminalizes the Task and enters
`cleanup_pending`. It does not return an error while leaving a retryable unbound Task.

Effective authority is checked again when execution is claimed. Current baseline
authority may authorize an unchanged candidate. Approval-backed authority must remain
exact, current, unexpired, and unrevoked. Runtime reconciliation continues to enforce
the frozen revocation deadline after activation.

## Approval delivery

Approval state and approval-channel delivery are separate facts.

The transaction that creates the runtime-bound approval also inserts one outbox record
whose stable idempotency identity is the approval UUID. Delivery records may move from
`pending` to `claimed` to `delivered`, with retry metadata. They never create another
approval.

The dispatcher supplies the stable approval identity to adapters that support
idempotency. An adapter without native idempotency must recover by querying a
Steward-authored correlation marker before creating anything. If neither behavior is
possible, the adapter cannot be used for authoritative approval delivery.

An HTTP submission retry does not file or re-file an approval. It returns the same Task
and current projected state while the reconciler/outbox completes independently.

## Activation

The `activation_pending` transaction is the durable authorization to replace the inert
desired state with the exact active manifest. The reconciler:

1. fetches by name and verifies the persisted UID and operation identity;
2. applies only the server-owned active desired fields with optimistic Kubernetes
   resource-version protection;
3. treats timeout or disconnect as ambiguous and observes rather than compensates;
4. waits for `AgentRuntime.status` to prove the exact generation and required provider
   set are ready; and
5. compare-and-sets orchestration state to `active`.

Provider readiness is set equality, not subset inclusion. Profiles required by the
binding and candidate must be attached with the expected binding identity. Every
Steward-managed profile absent from the desired set must be detached before readiness
is true.

No command may start while orchestration is earlier than `active`, the runtime UID or
manifest generation differs, or authority revalidation fails.

## Canonical flows

### Baseline Task

1. The API transaction records the immutable candidate, baseline evaluation, and
   `intent_recorded`; the response exposes the contract-defined submitted state.
2. The orchestrator authorizes and creates the inert runtime.
3. It observes and persists the exact UID.
4. A locked transaction re-evaluates the unchanged candidate against the latest
   Envelope and records the exact activation authority.
5. The orchestrator applies the active manifest and waits for exact provider-set
   readiness before entering `active`.
6. Inputs and an execution command may already be durable; execution is claimed only
   after `active` and another authority check.

### Approval-backed Task

1. The API transaction records the immutable excessive candidate and deltas and
   projects the Task as parked. It creates no runtime-bound approval yet.
2. The orchestrator creates the same inert zero-authority runtime and persists its UID.
3. One transaction creates the append-only admission decision, one approval, and one
   outbox event bound to that UID.
4. A pending decision causes no runtime mutation. Rejection or stale authority enters
   cleanup without creating another approval.
5. Approval creates exact UID- and Envelope-revision-bound grants. The orchestrator
   revalidates them transactionally before authorizing activation.
6. Only then does it apply the active manifest and eventually release a requested
   execution to the queue.

### Finalization during provisioning

1. The API appends a monotonic finalization command and returns; it does not delete.
2. The orchestrator compare-and-sets `cleanup_pending`, invalidating every stale
   provisioning or activation generation.
3. If creation was ambiguous, it resolves the exact inert operation until a UID is
   known or the operation is definitively absent.
4. It tears down only the observed UID and completes finalization only after exact
   projection absence and final evidence are durable.

## Execution attempts

The public Task remains the single execution atom. Internally, Steward records one
non-public execution attempt so crash recovery does not turn an ambiguous call into a
second execution.

The attempt record contains:

- a server-authored attempt UUID unique per Task;
- exact Task UID, runtime UID, active manifest digest, command digest, and input digest;
- `start_pending`, `running`, `succeeded`, `failed`, `cancel_pending`, or
  `outcome_unknown` state;
- adapter observation identity and bounded retry metadata; and
- result digest/reference when available, never credentials.

Execution proceeds as follows:

1. In one transaction, revalidate authority, require `active`, create the unique
   `start_pending` attempt, and consume the execution command.
2. Invoke the adapter with the attempt UUID as an idempotency key.
3. Observe the same attempt by UUID. A response alone is not durable proof.
4. Commit `running` only from adapter acknowledgement for that UUID.
5. Commit terminal result and Task phase together, then enter cleanup when requested by
   the active contract.

The runtime adapter contract must provide idempotent start and observation by attempt
UUID, including durable result retrieval after controller restart. If the underlying
runtime cannot provide this, Steward must not retry an ambiguous start. It records
`outcome_unknown`, disables authority, and cleans up. A deliberate rerun is a new Task
UID under the public contract.

The vendor-neutral execution port therefore needs separate operations equivalent to:

```text
start_task(attempt_id, exact_runtime_uid, command_and_input_digests)
observe_task(attempt_id) -> absent | accepted | running | terminal(result_ref)
cancel_task(attempt_id)
```

The current synchronous run-and-return shape is insufficient for restart-safe
execution. Conformance must establish the pinned runtime's actual idempotency and
observation behavior. If OpenShell cannot supply it, the adapter must add a durable
attempt marker/result boundary inside the exact sandbox or use the fail-closed
`outcome_unknown` path; implementation must not infer upstream behavior.

Cancellation is a durable command. The reconciler first prevents any new start,
requests cancellation for the exact attempt if one exists, disables authority, and
then enters cleanup. A transport interruption never abandons or implicitly retries an
attempt.

## Finalization and cleanup

Finalization is a monotonic command, not an immediate delete.

The transaction accepting cancellation, authority loss, terminal failure, or explicit
finalization prevents future activation/execution and enters `cleanup_pending`. From
that point, no stale worker may move the Task back to approval, activation, or active
state because every such compare-and-set includes the older generation and state.

For a Task-owned runtime with an observed UID, cleanup:

1. requests the inert/terminating desired provider set so omitted model, tool,
   credential, and network projections are detached;
2. waits for the AgentRuntime finalizer to record authority-disabled observations;
3. deletes the CR using the exact UID as a Kubernetes precondition;
4. observes that the exact UID is absent; and
5. commits final evidence and `finalized` together.

For `runtime_create_pending` with an ambiguous create, cleanup first resolves the create
idempotently. It is acceptable to complete creation of the inert zero-authority object
solely to obtain its UID and then delete it. This is safer than marking finalization
complete while an earlier create may still appear.

For a Task with no authorized or attempted runtime effect, cleanup may transition
directly to `finalized`. For an adopted or resident runtime, Task finalization removes
only Task-owned execution and authority projections; it never deletes the shared
runtime.

A name lookup returning a different UID proves that the original Kubernetes CR
incarnation no longer occupies that name, but it does not by itself prove that every
external projection for the old UID was revoked. The replacement never satisfies Task
readiness and never authorizes touching it. Final evidence joins teardown observations
to the original exact UID. Orphan scanning may report foreign or inconsistent objects,
but a global or name-only reaper is forbidden.

## Durable data model

The implementation should separate immutable Task intent from mutable orchestration
progress. The physical names below are prescribed for the implementation unless a
migration review identifies a PostgreSQL constraint that requires an equivalent shape.

### `task_submissions`

Keep the public Task aggregate and immutable snapshots here:

- Task identity and idempotency identity;
- canonical principal and owner;
- TaskDefinition/workflow and Envelope pins;
- immutable candidate and command/input digests;
- immutable execution binding;
- public Task phase and terminal failure code;
- monotonic execute/cancel/finalize commands; and
- result and finalization projection.

API mutation methods may only fill immutable input slots once or advance monotonic
commands. They cannot write runtime UID or orchestration progress.

### `task_runtime_operations`

Add one authoritative orchestration row per Task requiring runtime coordination:

| Column | Contract |
|---|---|
| `task_uid` | Primary key and foreign key to the immutable Task |
| `operation_id` | Unique immutable server UUID used for external correlation |
| `state` | One of the eight orchestration states above |
| `generation` | Positive, monotonically increasing compare-and-set version |
| `runtime_ownership` | Provisioned or adopted/resident cleanup semantics |
| `runtime_namespace`, `runtime_name` | Server-authored immutable rendezvous address |
| `inert_manifest_digest`, `active_manifest_digest` | Exact canonical desired-state identities |
| `runtime_uid` | Nullable until observed, then non-empty and immutable |
| `runtime_resource_version` | Latest observed Kubernetes concurrency token |
| `activation_authority_kind` | Nullable, then `baseline`, `grant`, or internal pinned authority |
| `activation_envelope_revision`, `activation_envelope_digest` | Exact authority observation used for activation |
| `approval_id` | Nullable exact approval identity for the grant path |
| `retry_at`, `last_error_code` | Bounded non-secret operational retry data |
| `lease_owner`, `lease_expires_at` | Optional duplicate-work reduction; never a correctness boundary |
| timestamps | Requested, observed, activated, cleanup requested, and finalized audit times |

Database constraints require runtime UID from `runtime_observed` onward, prohibit it in
`intent_recorded`, require approval identity in `approval_pending`, require activation
authority in `activation_pending` and `active`, and require finalized evidence in
`finalized`. A trigger rejects mutation of operation ID, runtime address, manifest
digests, and a non-null runtime UID. Another trigger or transition function rejects
illegal state edges.

Existing `task_submissions.runtime_uid` becomes a compatibility projection obtained by
joining the operation row. It must not remain a second writable source of truth.

### `task_execution_attempts`

Add at most one internal attempt per single-shot Task. Attempt identity, runtime UID,
input/command/manifest digests, and adapter correlation are immutable. State changes
use their own generation compare-and-set. This table is internal and does not create a
public Run resource.

### `external_effect_outbox`

Use an outbox for approval filing and any other external notification that cannot be
derived solely by reconciling Kubernetes desired state. The business transaction and
outbox insert commit together. Delivery is at least once by stable effect UUID;
external creation must be idempotent by that UUID.

### Journal

Append an orchestration event for every successful state transition, including
generation, effect identity, safe outcome code, and timestamp. Never log credentials,
tokens, raw provider output, or secret-bearing request bodies. The journal is audit and
evidence; the current operation row is the reconciliation source.

## Store transition API

Store methods should express domain outcomes instead of returning only
`InvalidTaskTransition`:

```text
Applied(current)
AlreadyApplied(current)
Superseded(current)
AuthorityInactive(current, reason)
InvariantViolation(safe_reason)
```

Required atomic operations include:

- reserve immutable Task plus `intent_recorded` operation;
- append input, execute, cancel, or finalize command;
- authorize inert create;
- record an observed exact UID;
- choose baseline activation or create runtime-bound approval plus outbox;
- consume an approved active grant and authorize activation;
- terminalize inactive authority and enter cleanup;
- record active desired state observation;
- claim one execution attempt after authority revalidation;
- commit execution terminal result;
- enter cleanup from any non-finalized state; and
- record exact-runtime absence, final evidence, and finalization.

Each operation locks the Task and orchestration row, validates expected generation and
state, and commits related Task phase/event changes together. External calls occur
outside database transactions.

If a database operation returns an error after a possible commit, the reconciler reads
the row by immutable Task/operation identity. It neither repeats a non-idempotent effect
nor deletes an external resource based on the error.

## Reconciler algorithm

The worker loop is level-based:

1. Select due, non-finalized operations, optionally with `FOR UPDATE SKIP LOCKED` to
   assign short leases.
2. Read the immutable Task, commands, orchestration generation, current authority, and
   relevant external observation.
3. Compute one pure next action.
4. Commit the next effect intent with a generation compare-and-set.
5. Perform at most one external effect.
6. Observe its exact identity/result.
7. Commit the observation with another compare-and-set.
8. On `Superseded`, discard the stale result and reconcile current state. Never apply a
   backward transition.

At least two controller replicas must be safe. A lease reduces duplicate calls, but
operation ID, Kubernetes identity labels, adapter idempotency, unique database keys,
and compare-and-set transitions provide correctness.

Work selection is driven by orchestration state and due time. It must not infer work
from combinations such as `runtime_uid IS NULL AND phase IN (...)`. Every non-finalized
state has either a next action, an explicit deadline, or a recorded blocking finding.

## Failure semantics

Failures are classified before state changes:

| Class | Examples | Required behavior |
|---|---|---|
| Definitive domain rejection | approval rejected, authority inactive, immutable collision | Record terminal Task outcome and `cleanup_pending` atomically |
| Retryable observation failure | Kubernetes unavailable, approval read timeout | Keep current state, set bounded retry metadata, perform no compensation |
| Ambiguous effect result | create/patch/start/delete response lost, database commit result unknown | Observe by immutable operation/attempt identity; never assume either outcome |
| Invariant violation | different UID at name, digest mismatch, multiple approvals | Fail closed, preserve foreign resources, record a finding, and clean up only exact owned identities |
| Execution outcome unknown | command may have started but adapter cannot observe it | Never replay; disable authority, record terminal safe failure, and clean up |

Retry counts are operational signals, not authority. Exhausting a retry budget does not
permit unsafe compensation. Explicit deadlines may terminalize work, but cleanup still
continues until absence is proven.

## Required fault-injection matrix

The implementation is incomplete until automated tests interrupt every boundary below.
Security-relevant negative tests are written first.

| Interruption or race | Required recovery assertion |
|---|---|
| Task transaction commits but API response is lost | Same submission key returns the same Task; API performs no side effect |
| `runtime_create_pending` commits before Kubernetes call | Reconciler creates exactly the inert runtime |
| Kubernetes create succeeds but response is lost | Reconciler finds matching operation identity and persists the same UID |
| Same name contains different Task/operation identity | Steward neither adopts nor deletes it; Task fails closed |
| UID observation commits but caller sees a database error | Re-read returns the UID; runtime is not deleted |
| Two reconcilers observe create pending | One logical runtime and one immutable UID result |
| Finalization races with create | Stale create cannot activate; inert runtime is resolved by UID and deleted |
| Envelope changes before baseline activation and still admits | Latest exact authority is recorded and activation proceeds once |
| Envelope changes before activation and rejects | Task terminalizes and cleanup begins; no create/delete retry loop |
| Approval row commits before channel delivery | Outbox files one externally correlated approval |
| Approval becomes terminal during restart | Original approval is resumed; no second approval exists |
| Grant expires, is revoked, or Envelope advances before activation | Runtime remains inert and cleanup begins |
| Activation patch succeeds but response is lost | Exact UID/digest observation advances to active once |
| Tool or model is removed from desired state | Former provider is explicitly detached before readiness |
| Execution start response is lost | Adapter observes the same attempt or Steward records outcome unknown; command is never replayed |
| Result exists but result commit response is lost | Same attempt/result digest is recovered without rerun |
| Finalization races with queued/running execution | No later activation/start wins; authority is disabled before teardown |
| Delete succeeds but response is lost | Exact UID absence completes finalization idempotently |
| Name is reused after exact UID deletion | Replacement is untouched and does not satisfy or block exact-UID evidence |
| Controller crashes in every state | Restart converges to the same allowed next state without human or HTTP retry |

The integration lane uses real Postgres and a controllable Kubernetes/runtime adapter.
The E2E lane uses the pinned OpenShell stack and proves inertness, exact UID binding,
provider detachment, revocation, no replay, and leak-free finalization. Teardown remains
unconditional and run-scoped.

## Migration and rollout

This architecture is an internal lifecycle replacement, not an online reinterpretation
of in-flight rows.

1. Add the new operation, attempt, outbox, constraints, and journal schema in a new
   migration; never edit an applied migration.
2. Add store transition APIs and reconcilers behind a staged mode that rejects new Task
   submission and does not let old and new lifecycle owners process the same Task.
3. Before activating the migration, require all legacy non-finalized Tasks to drain or
   be explicitly finalized. The migration must fail if an in-flight legacy Task exists.
4. Backfill finalized historical Tasks as read-only history. Do not manufacture runtime
   UIDs, bindings, authority, or execution attempts.
5. Roll all API and controller replicas to staged mode and verify no old writer remains.
6. Activate the new writer in a separate deployment change. From that point, API
   submission creates Task intent plus orchestration row atomically.
7. Remove the old API-side runtime and approval side-effect paths only after fault
   injection, integration, E2E, migration, and rollback gates are green.

Rollback first disables new submission, then drains/finalizes new-state Tasks with the
new controller. Old controllers must never see claimable new orchestration rows.

## Implementation slices

The development team should implement this as one architecture change with reviewable
internal slices, not as independently deployable partial semantics:

1. Schema, state-transition types, constraints, and pure transition tests.
2. Immutable API intent reservation and side-effect-free idempotent retry.
3. Inert runtime manifest, Kubernetes correlation, UID observation, and concurrent
   reconciliation.
4. Baseline authority and approval/outbox paths after UID observation.
5. Activation readiness and desired provider-set reconciliation.
6. Idempotent observable execution-attempt adapter contract.
7. Cancellation, authority loss, exact-UID cleanup, and final evidence.
8. Migration staging and complete fault-injection/E2E matrix.

No slice is production-activatable alone. In particular, inert creation without exact
cleanup, approval recovery without current-authority validation, or execution claiming
without no-replay recovery would expose the same partial-state hazards this design is
intended to remove.

### Repository change map

| Area | Required change |
|---|---|
| `migrations/` | Add the new orchestration, attempt, outbox, transition constraints, journal, and staged-rollout fence in a new migration after `0027`; do not edit existing migrations |
| `crates/steward-store` | Replace nullable-field work inference and generic invalid transitions with locked, generation-checked domain transitions and operation-state work selection |
| `crates/steward-apiserver/src/tasks.rs` | Reduce submission/retry to validation, immutable resolution, reservation, command append, and projection reads; remove runtime creation, approval filing, activation, binding recovery, and cleanup |
| `crates/steward-controller` | Make the Task orchestrator the single lifecycle owner; reconcile one effect intent/observation at a time and keep AgentRuntime status ownership separate |
| `crates/steward-ports` | Replace synchronous run-and-return with idempotent start, observe, and cancel attempt operations without vendor-shaped core types |
| `adapters/openshell` | Implement attempt correlation/observation, inert-first behavior, exact binding checks, and desired provider-set attach/detach reconciliation |
| approval adapter | Deliver the durable outbox identity idempotently and return correlation observations without creating authority independently |
| `e2e/` and `xtask/` | Add the fault matrix, two-reconciler races, real pinned-stack no-replay checks, migration fence, and leak-free teardown gates |

Internal orchestration enums need not enter the CRD or public API schema. Keep them in
the store/controller boundary unless another consumer has a demonstrated need. The
public Task projection is derived from the Task aggregate plus orchestration row.

## Definition of done

The architecture is implemented only when:

- API Task submission and retry code has no runtime repository, decision-channel, or
  execution side-effect dependency;
- exactly one Task orchestrator owns every Task lifecycle effect;
- all provisioned runtimes begin inert and no authority exists before durable UID
  observation;
- every authority activation and execution claim is an atomic current-authority
  decision;
- all external effects have immutable idempotency/correlation identities;
- cleanup is exact-UID-based and finalization proves projection absence;
- the public phase/evidence mapping remains compatible with the frozen M1 contract;
- no automatic path replays an ambiguous execution;
- database constraints reject invalid state combinations and backward transitions;
- all fault-injection cases above pass with at least two reconcilers; and
- the old API-side orchestration and nullable-field work inference paths are removed.

## Closed alternatives

- **Add another recovery conditional.** Rejected because it still reconstructs missing
  effect history and leaves another ambiguous boundary.
- **Create an active runtime and bind it afterward.** Rejected because authority may
  exist without a durable exact UID owner.
- **Approve before runtime creation.** Rejected because grants bind to the
  Kubernetes-assigned runtime UID.
- **Use deterministic name as identity.** Rejected because names are reusable.
- **Delete after any binding error.** Rejected because the database transition may have
  committed and the returned error may be ambiguous.
- **Let HTTP retry resume orchestration.** Rejected because recovery cannot depend on a
  client returning and creates multiple lifecycle writers.
- **Use a controller lease as the safety boundary.** Rejected because leases expire and
  cannot make external effects atomic.
- **Automatically rerun after an ambiguous execution.** Rejected because model/tool
  effects may already have occurred.
- **Introduce an external workflow engine.** Rejected for this scope. A Postgres-backed
  reconciler is sufficient once effect intent and observation are modeled explicitly.
