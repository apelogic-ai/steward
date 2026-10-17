# Calling Steward from Jenkins and Temporal

Status: **Current implementation for the upcoming Steward v0.3.20 release**

Jenkins pipelines and Temporal activities can start governed runs through the
same Run API used by other service callers. They authenticate as deployment-
configured services; a request cannot choose its service name or owner.

The machine-readable contract is the generated
[Run API OpenAPI document](contracts/run-api/openapi.json). This page explains
the caller sequence and the token rules that are easier to miss when reading
individual operations.

## Before you call the API

An administrator must configure all of the following:

- one stable service name for each caller system, such as `jenkins-ci` or
  `temporal-worker`;
- the canonical owner and active User Envelope for that service;
- a versioned Workflow that the service may invoke, referenced as
  `"name@N"`, for example `"release-summary@1"`; and
- a trusted token-exchange path that issues `steward-task-v2` tokens with the
  exact Steward audience.

Do not reuse one service name for Jenkins and Temporal. Separate names keep
run evidence, limits, and incident response attributable to the system that
started the work.

Jenkins uses an OpenID Connect assertion from the Jenkins OIDC Provider plugin
and exchanges it at Identity's generic OIDC endpoint. This integration requires
the **next `github-oidc-exchange` release** after the merged generic OIDC work;
it is not part of the current released version. The Identity policy binds the
exact Jenkins issuer, audience, job subject, and optional signed claims to the
service and owner. The request body cannot override that binding. There is no
API-key fallback.

A Temporal worker obtains its Task token through the identity mechanism for
its deployment. An in-cluster worker normally uses the configured workload
exchange; an external worker can use an administrator-configured OIDC provider
when that integration is available. Construct the token source when the worker
starts and inject it into the activity implementation. Never put a source OIDC
assertion or Steward Task token in Workflow or Activity inputs, results,
search attributes, memo, heartbeat details, or logs.

## Token exchange and lifetime

Token exchange accepts a source assertion in the `Authorization` header and an
empty request body. A successful response contains an `access_token`,
`token_type: "Bearer"`, and its lifetime in `expires_in`. Keep both the source
assertion and returned token in memory or a private, short-lived file, disable
shell tracing, and delete any file before the step finishes.
Pin the exact HTTPS Identity and Steward origins and refuse redirects so an
authorization header cannot be forwarded to another endpoint.

Steward rejects a Task token more than 300 seconds after it was issued, even if
another claim would otherwise keep it valid. Current Identity Task tokens are
shorter-lived. Obtain a fresh source assertion and exchange it again for every
poll instead of retaining a token while a run executes. Also exchange again
before downloading output or requesting cleanup after a long run.

## Run sequence

Use this sequence for every run:

1. Exchange the caller credential for a Steward Task token.
2. `POST /v1/tasks` with `Content-Type: application/json`, the bearer token,
   and a stable `Idempotency-Key`. For a published Workflow, the request is
   `{"workflow":"name@N"}`. To identify the run in HyperShell, add
   `"callerReference":{"label":"build #142","url":"https://ci.example.com/builds/142"}`.
   The label is limited to 128 UTF-8 bytes and the optional URL must be HTTPS
   and at most 2,048 bytes. Do not put credentials or secret values in either
   display field. A new Task returns `202`; an exact retry returns
   `200` and the existing `taskUid`. A changed request with the same key returns
   `409`.
3. Create a tar archive for the inputs. When there are no input files, upload a
   valid empty tar archive, not a zero-byte body.
4. `PUT /v1/tasks/{taskUid}/inputs` with `Content-Type: application/x-tar`.
   Upload and execute are idempotent, so retrying this sequence after an
   uncertain response is safe when the bytes and Task are unchanged.
5. `POST /v1/tasks/{taskUid}/execute`.
6. Poll `GET /v1/tasks/{taskUid}` with a newly exchanged token on every poll.
   Continue for `submitted`, `queued`, or `running`; stop for `succeeded`,
   `failed`, or `cancelled`.
7. Read `GET /v1/tasks/{taskUid}/outputs`. It returns:
   - `200 application/x-tar` for a successful result, or for a failed run with
     validated partial output or requested bounded diagnostics;
   - `204 No Content` when a failed run has nothing caller-visible; or
   - `409 Conflict` while the run is not terminal.
8. Read `failureReason` from the terminal status when the phase is `failed`.
   It is the bounded explanation intended for callers.
9. In a `finally` path, exchange a fresh token and
   `DELETE /v1/tasks/{taskUid}`. A `202` response means cleanup was requested;
   the status field `finalized` reports when cleanup is complete.

The input and output archives use workspace-relative paths. Validate downloaded
tar entries before extraction: reject absolute paths, `..`, symlinks, and any
entry that would escape the chosen output directory.

## Idempotency

Jenkins should derive one stable key from immutable job identity, for example
`jenkins:<job-full-name>:<build-number>`. Use the same key only when retrying
that exact request.

`SubmitRun` in Temporal derives its key from the Activity execution:

```text
ns:workflowId:runId:activityId
```

The namespace, Workflow ID, Workflow Run ID, and Activity ID remain stable
across retries of that Activity. Do not add the Activity attempt number, a
timestamp, or a random value: those would turn a retry into a second run.

## Jenkins sample

[`examples/callers/Jenkinsfile`](examples/callers/Jenkinsfile) shows a
shared-library-style `stewardRun` step. Move that function into
`vars/stewardRun.groovy` in the pipeline library and keep the short invocation
in the consuming Jenkinsfile.

The step allocates an agent only for creating/uploading the tar and downloading
the result. Its `timeout { waitUntil { ... } }` polling block is outside
`node {}` so a long governed run does not reserve a Jenkins executor. Each
`waitUntil` iteration enters the OIDC credential binding again, which causes
the Jenkins plugin to issue a fresh source token, then performs a new exchange.

## Temporal Go sample

[`examples/callers/temporal-go/activities.go`](examples/callers/temporal-go/activities.go)
defines `SubmitRun` and `AwaitRun` activities. Register an `Activities` value
whose `Tokens` and `Results` dependencies are constructed in the worker
process. The sample deliberately keeps credentials out of serializable input
and result structs.

`AwaitRun` records only the non-secret phase in Activity heartbeat details. It
streams an output tar to a caller-owned result store and returns the resulting
non-secret reference rather than placing a potentially large archive in
Temporal history.

## Refusals to preserve

- A missing, expired, replayed, wrong-audience, or incorrectly signed source
  assertion must fail at exchange; do not retry it as an API key.
- A Task token bound to another service or owner must not be rewritten or
  supplemented by request fields.
- A caller reference is display text only. Steward derives the displayed
  service name from the verified token and refuses a request field that tries
  to select it.
- `404` on a Task read or output request does not reveal whether another owner
  has that Task UID.
- `409` on outputs means the Task is not finished; it is not an empty result.
- `204` is an empty caller-visible result for a terminal failed run; preserve
  the terminal phase and `failureReason` in caller evidence.
- Never print an exchange response, bearer header, source assertion, or token.
