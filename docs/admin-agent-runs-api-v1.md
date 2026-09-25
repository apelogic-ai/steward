# Steward administrator Agent Runs API v1

Status: source-backed browser contract. This document defines the server
contract for the Agent Runs dashboard. It deliberately distinguishes
recorded facts from desired configuration and from data Steward does not
persist. The browser must never fill an unavailable field from a heuristic.

## Authority and privacy boundary

User operations are below `/app/api/v1/runs`; reads are restricted to the exact
canonical owner and mutations additionally require the browser CSRF proof.
Administrator reads are below `/admin/api/v1/all-runs` and require exact
browser administrator authority. A member-role identity, Task identity,
runtime identity, provider credential, or Kubernetes bearer token is not
browser authority.

The run read model contains no input or output archives, command arguments,
provider payloads, raw logs, prompts, model output, tokens, credentials,
assertions, HTTP headers or bodies. A Task failure is reduced to a bounded
category; the stored free-form failure reason is not returned. Execution logs
are available only through the separate bounded stream endpoint.

## Source and gap matrix

| Field | Source | Freshness | v1 representation |
|---|---|---|---|
| Canonical run ID | `task_submissions.task_uid` | Durable | Available. This identity is never replaced by an external correlation. |
| Workflow and coding-agent runtime | `task_submissions` | Submission snapshot | Available. |
| Submitter service, acting user and owner | `task_submissions` | Submission snapshot | Available under administrator authority. |
| Runtime UID and ownership | `task_submissions` | Durable binding | Available after binding; otherwise explicitly unavailable. |
| Current Task phase and finalization | `task_submissions` | Current durable state | Available with `updatedAt`. |
| Lifecycle timeline | append-only `task_lifecycle_events` | Recorded transactionally after migration | Complete for newly recorded Tasks. Migrated Tasks are explicitly `partial` because intermediate historical transitions cannot be reconstructed. |
| User Envelope revision | immutable Task authority snapshot | Submission snapshot | Available for v0.2 user Tasks; historical legacy-authority Tasks report unavailable. |
| Configured models and tool grants | immutable `task_submissions.runtime_spec` snapshot | Submission snapshot | Available as configured authority only, never described as calls. |
| Budget allocation | `task_submissions.runtime_spec.budget` | Submission snapshot | Available. |
| Observed spend | latest append-only `spend_observations` row joined by `runtime_uid` | Observation timestamp | Available when observed; otherwise unavailable. Spend is observed, never custodied. |
| Runtime-minute authority and usage | immutable User Envelope snapshot plus append-only `task_lifecycle_events` and runtime-minute observations/grants | Envelope-instance period/current observation | Available when the Envelope sets a limit. Usage is derived from running-to-terminal intervals clipped to the UTC month; it is not inferred from runtime age or added to the AgentRuntime CRD. |
| Inference calls, actual model, input/output tokens | Not persisted by Steward | Unknown | Explicitly unavailable. Configured models are returned separately. |
| Tool calls and outcomes | Not persisted by Steward | Unknown | Explicitly unavailable. Granted tools are returned separately. |
| Runtime CPU, memory, storage and network use | Not persisted by Steward | Unknown | Explicitly unavailable. |
| GitHub repository/workflow/run URL | validated `DirectTaskBindingEvidence.sourceProvenance` submission snapshot | Submission snapshot | Available for direct GitHub Tasks; otherwise absent. The submitter idempotency key is never interpreted as GitHub metadata. |
| Stages | append-only lifecycle stage events plus current Task binding/state | Transactional/current | Admission, runtime binding, execution, and finalization are available. |
| Execution step | bounded run presentation | Current | One agent-execution step names its stdout/stderr streams. Fine-grained agent steps are not claimed. |
| Failure | `task_submissions.failure_reason` | Terminal Task snapshot | A bounded category only; the stored reason is never returned. |

`runtime_events` is not used for this API. It is not populated by the Task
controller and current runtime phase remains a CRD-status concern. Treating it
as a Task timeline would create an unaudited join and an incorrect source of
truth.

## Operations

### `GET /app/api/v1/runs` and `GET /admin/api/v1/all-runs`

Returns newest-first runs ordered by immutable `(created_at, task_uid)`. The
optional `cursor` is the last Task UUID from the preceding page. The store
resolves its immutable creation boundary, so concurrent phase or spend updates
cannot move a row between pages. An unknown or malformed cursor fails closed.

Supported query parameters:

- `limit`: 1 through 100; default 50;
- `cursor`: Task UUID;
- `phase`: one Task phase;
- `workflow`: exact non-empty workflow name;
- user list only: `runtimeUid` and `envelopeInstanceId`;
- administrator list only: `runtimeUid` and opaque `ownerUserId`.

Unknown query parameters are rejected.

Each response includes counts for every Task phase. Counts use the current
query with `phase` removed so phase chips remain useful while one phase is
selected. Administrator rows also include `ownerDisplayEmail` under the same
administrator authority as the list.

### Run detail and timeline

`GET /app/api/v1/runs/{taskUid}` and
`GET /admin/api/v1/all-runs/{taskUid}` return one canonical Steward Task read
model. A valid but absent Task UUID is `404`. The response uses the same summary
shape as the list and never resolves an external run in place of the Task UUID.

The corresponding `/timeline` routes return lifecycle events in `(at, id)`
order. Events include phase/finalization changes and bounded structured stage
events such as `admitted`, `runtimeBound`, `executionStarted`, and
`executionEnded`. Consumers must not invent missing transitions.

### Execution logs

`GET .../{taskUid}/logs/{stream}?after={byteOffset}` accepts only `stdout` or
`stderr`. It returns `{ stream, content, truncated, sizeBytes, complete }` and
at most 64 KiB per read. A live OpenShell transcript is readable before the
Task becomes terminal; terminal captured output remains readable afterward.
The owner/admin scope of the parent route applies unchanged.

For compatibility with the original endpoint, a request that omits `after`
receives the complete captured stream as `text/plain; charset=utf-8`. Both
representations are `no-store` and `nosniff`; new clients use the bounded JSON
representation with an explicit byte offset.

### Cancel and re-run

`POST /app/api/v1/runs/{taskUid}/cancel` transitions the caller's eligible Task
to `cancelled`. `POST /app/api/v1/runs/{taskUid}/rerun` takes an idempotency key
and creates a fresh Task against the same active Envelope instance for a
versioned Steward workflow. For a direct GitHub Task, it invokes GitHub's re-run
operation through the caller's governed connection with the exact
`github/actions_run_trigger/write` authority. Steward returns `202` while it
waits for a Task whose validated provenance has the same repository and GitHub
run ID and a higher attempt; browser retries reuse the same idempotency key.
This works for any valid workflow filename. Cloning the Task, copying old
provenance, or treating an idempotency key as provenance is forbidden.

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
- GitHub correlation writes validate structured repository, workflow, actor,
  ref, SHA, run ID/attempt, and caller-workflow fields. They never reinterpret
  `idempotency_key`.
- Live diagnostics are a separate bounded contract; this API remains useful
  when they are absent.
