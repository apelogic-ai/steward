# Durable Task runtime orchestration

Status: Steward v0.2 implementation contract.

This document describes the recoverable boundary between Task intent, the Task orchestrator, and
the `AgentRuntime` controller. Direct Git packages and immutable versioned Workflows are the only
supported external Task paths.

## Authority classes

External user Tasks have one authority: the exact active, provisioned User Envelope owned by the
authenticated canonical user and selected at submission. Steward validates the effective runtime
against the approved snapshot and persists:

- canonical owner user ID;
- Envelope request/instance ID;
- Envelope revision and digest;
- exact approved Envelope snapshot;
- authority kind `user-envelope`;
- admission result and complete effective runtime requirements.

Internal product operations, currently governed Connections, use an exact versioned code-owned
authority identified by ID, version, and digest. It is not administrator-authored and cannot
authorize an external Task.

The deployment capability catalog advertises operationally selectable models and tools to the
administrator editor. It never participates in Task admission or controller recovery.

## State ownership

| State | Owner | Source of truth |
|---|---|---|
| Task business phase and immutable authority evidence | API/store | PostgreSQL Task aggregate |
| Orchestration progress and effect journal | Task orchestrator | PostgreSQL orchestration record |
| Current runtime phase and provider projections | AgentRuntime controller | `AgentRuntime.status` and exact runtime UID |

The API writes immutable intent and monotonic commands. It does not create, activate, execute, or
delete a runtime. The Task orchestrator owns those external effects. The AgentRuntime controller
owns runtime status and adapter projections.

## Orchestration v3

Every new Task uses orchestration version 3 and carries exactly one complete authority form:

```text
user-envelope:
  owner_user_id
  user_envelope_instance_id
  user_envelope_revision
  user_envelope_digest
  user_envelope_snapshot

internal:
  internal_authority_id
  internal_authority_version
  internal_authority_digest
```

Mixed or incomplete records are rejected by the store. User Tasks must have no internal pins;
internal Tasks must have no User Envelope pins. Historical terminal v1/v2 records remain readable
but cannot regain live authority.

The orchestration state machine remains:

```text
intent_recorded
  -> runtime_create_pending
  -> runtime_observed
  -> activation_pending
  -> active
  -> cleanup_pending
  -> finalized
```

`approval_pending` is retained only for historical decoding. New v3 Tasks do not enter it.

## Reconciliation rules

1. Before runtime creation, re-read the exact provisioned User Envelope record and verify owner,
   instance ID, revision, digest, status, and approved snapshot against the immutable Task pins.
2. Construct the inert runtime from the persisted Task spec and authority snapshot.
3. Observe and persist the exact Kubernetes runtime UID before any runtime-bound effect.
4. Revalidate the same pinned authority before activation. A revoked, stale, replaced, or
   otherwise inactive User Envelope sends the Task to cleanup; no newer Envelope is substituted.
5. Apply the already-admitted spec and persist activation authorization before the external
   activation effect.
6. Claim one durable execution attempt. Ambiguous execution outcome fails closed; it is never
   silently replayed.
7. Finalization removes Task-owned runtime and provider projections and completes only after their
   absence is observed.

Internal Connection operations follow the same effect discipline but validate their exact
persisted code-owned authority instead of querying User Envelopes.

## Recovery guarantees

- Restart, leader change, lease expiry, and retry use the same immutable Task authority.
- A template or capability-catalog change cannot alter an existing Task.
- A newly provisioned or broader User Envelope cannot rescue an old Task whose pinned authority is
  inactive.
- A narrower or revoked pinned Envelope prevents effects that have not yet occurred.
- Runtime names are locators only; runtime UID is the identity for observations and cleanup.
- Provider reconciliation is set-based: omitted managed providers are detached.
- Finalization never invents success and never uses current mutable configuration as authority.

## Upgrade from v0.1.23

Migration `0039_user_envelope_only_task_authority.sql` is an offline writer boundary:

1. Stop or stage v0.1.23 writers.
2. Finalize every unfinished legacy Task that lacks complete User Envelope or internal-authority
   pins. The migration rejects the database while any such Task remains.
3. Run the migration. It upgrades unfinished exactly pinned Tasks to v3, recovers the exact
   approved User Envelope snapshot from its provisioned request event, and clears the historical
   global-authority digest from live records.
4. Deploy all v0.2 binaries before returning orchestration to `active`.

Terminal historical records remain v1/v2 and readable. They cannot be resumed, widened, or used
as current authority.

Rollback is safe before migration. After migration but before any v0.2-only Task is admitted,
rollback requires restoring the pre-migration database as well as v0.1.23 binaries. After a v0.2
Task exists, image-only rollback is unsupported because v0.1.23 cannot interpret orchestration v3
authority. Complete or remove v0.2 work under an explicit recovery plan before restoring both the
database and binaries.

## Required evidence

The release gate exercises:

- direct and versioned admission with only an exact User Envelope;
- missing, foreign, over-limit, stale, and revoked User Envelope rejection;
- restart and retry against immutable v3 evidence;
- internal Connection authority without any user-authority fallback;
- v0.1.23 migration refusal and exact-pin upgrade;
- runtime cleanup and provider detachment;
- chart rendering with a descriptive capability catalog and no global Task authority.
