# Task production configuration

The Task API is part of `steward-apiserver`; it is not a separately deployed service. The Task
worker is part of the normal `steward-controller` process.

## Apiserver inputs

| Input | Supported production value |
|---|---|
| `STEWARD_TASK_TOKEN_AUDIENCE` | Required and non-empty. Use `steward-task-api`. |
| `STEWARD_TASK_WORKFLOWS_JSON` | Required JSON array matching `workflows.example.json`. Commands are server-selected; clients cannot supply them. |
| `STEWARD_APISERVER_BIND` | HTTPS listener, default `0.0.0.0:8443`. Expose the existing apiserver Service port to this target port. |
| Task API enablement | Enabled whenever the production apiserver starts. There is no bypass flag; invalid or absent Task configuration fails startup. |
| `STEWARD_DATABASE_URL` | Existing Postgres connection reference. Keep the value in a Secret, never this repository. |

The caller supplies `Authorization: Bearer <exchanged-token>`. GitHub requests the production
exchange service's audience; Steward never receives the raw GitHub OIDC token. Steward sends the
exchanged token to TokenReview with audience `steward-task-api`. See
`docs/task-submission-api.md` for the required verified username/groups and the external mapper
boundary.

## Controller inputs

Task execution is enabled in the normal controller composition root when
`STEWARD_DATABASE_URL`, `STEWARD_OPENSHELL_ENDPOINT`, and the existing inference-plane inputs
are present. `STEWARD_S0_BOOTSTRAP=1` is the bootstrap-only mode and does not run the durable
Task worker; do not use it for a Task deployment. No second Task controller flag or service
port exists.

## Migration 0011

Both production composition roots run the append-only SQL migrator during startup.
`0011_task_submissions.sql` must be present in the deployed binary before either Task endpoint
or Task worker is enabled. Production rollout should run the repository's normal migration
gate first:

```sh
cargo xtask migrate-check
```

The migration creates durable Task lifecycle state and does not change the AgentRuntime CRD.

## Workflow and service envelope

`workflows.example.json` is a production-safe copy-smoke workflow catalog value. It grants no
LLM or tool access, has a zero budget, and performs only:

```sh
mkdir -p "$STEWARD_OUTPUT_DIR/out"
cp in/payload.bin "$STEWARD_OUTPUT_DIR/out/payload.bin"
```

The input and output names are workspace-relative tar paths. This smoke requires no LiteLLM or
MCP call. Inject the JSON through a ConfigMap or equivalent configuration source as
`STEWARD_TASK_WORKFLOWS_JSON`; do not let submitters override its command, namespace, models,
tools, budget, or TTL.

### API archive contract

Steward's copy-smoke contract defines only workspace-relative members in the opaque API tar
archives:

- Input tar member: `in/payload.bin`
- Output tar member: `out/payload.bin`

Artifact upload, download, and runner staging layouts are owned by the API consumer and its
deployment workflow, not by Steward.

Before enabling `steward-run`, a route-scoped bootstrap identity must author a service envelope
for the exact service name `steward-run`. `steward-run-service-envelope.example.json` is the
matching example and can be submitted to:

```text
POST /admin/service-envelopes/steward-run
```

If the workflow exceeds that envelope, submission returns `202` and parks. If no service
envelope exists, submission fails closed.

The checked-in envelope is authority-minimal: empty LLMs and tools, a `0.00 USD` monthly limit,
and a one-hour TTL. Bootstrap it over authenticated HTTPS with a short-lived route-scoped token
held only in a temporary file:

```sh
STEWARD_APISERVER_URL=https://steward.example.com \
STEWARD_APISERVER_CA_CERTIFICATE_FILE=/path/to/ca.crt \
STEWARD_SERVICE_ENVELOPE_BOOTSTRAP_TOKEN_FILE=/path/to/short-lived-bootstrap-token \
scripts/bootstrap-task-copy-smoke.sh
```

The procedure is idempotent. The first exact revision returns `201`; an identical retry returns
`200` and performs no write. An existing different revision remains a conflict and must be
reviewed rather than overwritten. The script requires explicit CA trust and HTTPS, keeps the
bearer token out of command arguments, and fails closed on every other response.

The production TokenReview contract for this credential is exact:

- audience: `steward-api`
- group, exactly once: `agents.apelogic.ai/service-envelope-bootstrap:steward-run`
- username: a non-empty identity verified by the cluster identity provider and preserved as the
  envelope's `authored_by` audit value

This identity can call only `POST /admin/service-envelopes/steward-run`. It is denied approvals,
grants, member envelopes, other service envelopes, and every other administrator route. Combining
the bootstrap group with the broad administrator group or a member-role group fails
authentication.

Bootstrap is blocked first on this Steward release containing the route-scoped authorization
contract, then on Infra providing a short-lived token through the DEV EKS OIDC identity-provider
association. Steward does not issue that token. It must not be stored in a Kubernetes Secret or
replaced with any other long-lived credential; the chart intentionally has no bootstrap-token
Secret input.

## Jira startup values

Jira is currently required whenever the production apiserver starts. DEV must supply these exact
inputs; no dummy value is valid:

| Environment input | Helm value/source | Required value shape |
|---|---|---|
| `STEWARD_JIRA_BASE_URL` | `config.apiserver.jiraBaseUrl` | Public HTTPS Jira tenant root, without a REST API suffix |
| `STEWARD_JIRA_PROJECT_KEY` | `config.apiserver.jiraProjectKey` | Existing project key used for Steward decisions |
| `STEWARD_JIRA_ACCOUNT_EMAIL` | `config.apiserver.jiraAccountEmail` | Account email corresponding to the API token |
| `STEWARD_JIRA_TOKEN` | Secret named by `secrets.jira.name`, key `secrets.jira.key` | Raw Jira API token |

The actual DEV tenant URL, project key, and account email are deployment inputs owned by GitOps;
the token is secret-store material. None belongs in this public repository. If Jira should become
optional, that requires a separate product/chart change rather than a placeholder credential.

## Release impact

This contract hardening changes Task identity parsing and makes service-envelope bootstrap
idempotent in the apiserver. A new signed patch release is therefore required after this change
merges; reuse of the previous Steward image/chart handoff is not sufficient.
