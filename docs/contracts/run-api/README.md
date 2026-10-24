# Steward Run API contract

Status: **Generated current Steward v0.3.25 contract**

The machine-readable contract is [`openapi.json`](openapi.json). The Steward
server generates that document from `steward_apiserver::ApiDoc`; the Run API is
the set of `/v1/tasks` paths in the document. Only that subset is the stable
caller contract. The same generated document also contains browser and
administrator routes because Steward exports one OpenAPI document; treat those
routes as internal unless their own documentation says otherwise.

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

A versioned-Workflow submission may include display-only caller context:

```json
{
  "workflow": "triage@1",
  "callerReference": {
    "label": "build #142",
    "url": "https://ci.example.com/builds/142"
  }
}
```

The label is 1–128 UTF-8 bytes from the documented display character set. The
optional URL is at most 2,048 bytes and must use HTTPS without credentials.
Steward records the service name from the verified Task token; request JSON has
no field that can select or replace it. The reference is display context only
and grants no access.

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
types, regenerate the document, and review the resulting contract diff. The
`committed_openapi_document_matches_generator` apiserver test runs in
`cargo xtask ci` and fails when the committed document differs from the
generator output.
