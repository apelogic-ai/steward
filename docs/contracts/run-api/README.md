# Steward Run API contract

Status: **Generated current implementation for the upcoming Steward v0.3.20 release**

The machine-readable contract is [`openapi.json`](openapi.json). The Steward
server generates that document from `steward_apiserver::ApiDoc`; the Run API is
the set of `/v1/tasks` paths in the document. The same generated document also
contains the server's browser and administrator routes because Steward exports
one OpenAPI document.

The generated `info.version` follows the repository package version. During
Unreleased development it therefore names the last release; the release PR
updates the package version and regenerates this artifact as one versioned
release change.

The Run API lifecycle is:

1. `POST /v1/tasks` with a stable `Idempotency-Key`;
2. `PUT /v1/tasks/{taskUid}/inputs` with an `application/x-tar` body, including
   a valid empty tar when the run has no inputs;
3. `POST /v1/tasks/{taskUid}/execute`;
4. `GET /v1/tasks/{taskUid}` until its phase is terminal;
5. `GET /v1/tasks/{taskUid}/outputs`; and
6. `DELETE /v1/tasks/{taskUid}` to request cleanup.

Every request requires a current Task bearer token. See
[`calling-steward-from-jenkins-and-temporal.md`](../../calling-steward-from-jenkins-and-temporal.md)
for token handling and complete caller examples.

## Regenerate

From the repository root:

```sh
cargo run --quiet -p steward-apiserver --example export-openapi -- \
  docs/contracts/run-api/openapi.json
git diff --exit-code -- docs/contracts/run-api/openapi.json
```

Do not edit `openapi.json` by hand. Change the server annotations or response
types, regenerate the document, and review the resulting contract diff.
