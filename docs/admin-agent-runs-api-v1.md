# Steward administrator Agent Runs API v1

Status: source-backed read-model contract. This document defines the read-only
server contract for the Agent Runs dashboard. It deliberately distinguishes
recorded facts from desired configuration and from data Steward does not
persist. The browser must never fill an unavailable field from a heuristic.

## Authority and privacy boundary

All operations are below `/admin/api/v1/runs` and use the existing exact
Steward administrator `RequestAuthenticator` and Kubernetes `TokenReview`
boundary. A member-role identity, Task identity, runtime identity, provider
credential, or the route-scoped service-envelope bootstrap identity has no
read authority.

Responses contain no input or output archives, command arguments, provider
payloads, raw logs, prompts, model output, tokens, credentials, assertions,
HTTP headers or bodies. A Task failure is reduced to a bounded category; the
stored free-form failure reason is not returned.

## Source and gap matrix

| Field | Source | Freshness | v1 representation |
|---|---|---|---|
| Canonical run ID | `task_submissions.task_uid` | Durable | Available. This identity is never replaced by an external correlation. |
| Workflow and coding-agent runtime | `task_submissions` | Submission snapshot | Available. |
| Submitter service, acting user and owner | `task_submissions` | Submission snapshot | Available under administrator authority. |
| Runtime UID and ownership | `task_submissions` | Durable binding | Available after binding; otherwise explicitly unavailable. |
| Current Task phase and finalization | `task_submissions` | Current durable state | Available with `updatedAt`. |
| Lifecycle timeline | append-only `task_lifecycle_events` | Recorded transactionally after migration | Complete for newly recorded Tasks. Migrated Tasks are explicitly `partial` because intermediate historical transitions cannot be reconstructed. |
| Envelope revision | nullable submission snapshot | Submission snapshot | Available for Tasks submitted after this contract; migrated Tasks report unavailable. |
| Configured models and tool grants | immutable `task_submissions.runtime_spec` snapshot | Submission snapshot | Available as configured authority only, never described as calls. |
| Budget allocation | `task_submissions.runtime_spec.budget` | Submission snapshot | Available. |
| Observed spend | latest append-only `spend_observations` row joined by `runtime_uid` | Observation timestamp | Available when observed; otherwise unavailable. Spend is observed, never custodied. |
| Inference calls, actual model, input/output tokens | Not persisted by Steward | Unknown | Explicitly unavailable. Configured models are returned separately. |
| Tool calls and outcomes | Not persisted by Steward | Unknown | Explicitly unavailable. Granted tools are returned separately. |
| Runtime CPU, memory, storage and network use | Not persisted by Steward | Unknown | Explicitly unavailable. |
| GitHub repository/workflow/run URL | No dedicated Task correlation record | Unknown | Explicitly unavailable. The submitter idempotency key is not interpreted as GitHub metadata. |
| Failure | `task_submissions.failure_reason` | Terminal Task snapshot | A bounded category only; the stored reason is never returned. |

`runtime_events` is not used for this API. It is not populated by the Task
controller and current runtime phase remains a CRD-status concern. Treating it
as a Task timeline would create an unaudited join and an incorrect source of
truth.

## Operations

### `GET /admin/api/v1/runs`

Returns newest-first runs ordered by immutable `(created_at, task_uid)`. The
optional `cursor` is the last Task UUID from the preceding page. The store
resolves its immutable creation boundary, so concurrent phase or spend updates
cannot move a row between pages. An unknown or malformed cursor fails closed.

Supported query parameters:

- `limit`: 1 through 100; default 50;
- `cursor`: Task UUID;
- `phase`: one Task phase;
- `workflow`: exact non-empty workflow name.

Unknown query parameters are rejected.

### `GET /admin/api/v1/runs/{taskUid}`

Returns one canonical Steward Task read model. A valid but absent Task UUID is
`404`. The response uses the same summary shape as the list and never resolves
an external run in place of the Task UUID.

### `GET /admin/api/v1/runs/{taskUid}/timeline`

Returns lifecycle events in `(at, id)` order. Event provenance is `recorded` or
`backfilled`. A response containing any backfilled event declares its history
`partial`; consumers must not invent the missing transitions.

## Availability model

Provider activity and correlation objects use one of:

- `available`: the named authoritative source supplied a value;
- `partial`: Steward has a bounded historical anchor but not the complete
  history;
- `unavailable`: Steward does not persist the value or no observation exists.

Every such object names its source and, when applicable, `observedAt`. An
unavailable object supplies a stable reason code, not a guessed value.

## Lifecycle and migration

The read model adds an append-only Task lifecycle table. A database trigger
records phase changes, finalization requests and finalization completion in the
same transaction as the Task row update. Migration anchors existing Tasks with
backfilled events but does not pretend to recover transitions that were not
stored. New Task reservations also snapshot the envelope revision used for
admission; existing rows retain `NULL` and surface that gap explicitly.

## Extension rules

- Add provider call summaries only when a reviewed persistence path records
  bounded metadata keyed to `task_uid` or its immutable `runtime_uid`.
- Never put raw logs, prompts, model output, request/response bodies or
  credentials into this read model.
- A future GitHub correlation write must validate structured repository,
  workflow and run fields. It must not reinterpret `idempotency_key`.
- Live diagnostics are a separate bounded contract; this API remains useful
  when they are absent.
