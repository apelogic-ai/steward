# Task production configuration

The Task API is part of `steward-apiserver`; it is not a separately deployed service. The Task
worker is part of the normal `steward-controller` process.

## Apiserver inputs

| Input | Supported production value |
|---|---|
| `STEWARD_TASK_ORCHESTRATION_MODE` | Required in both the apiserver and controller. `staged` rejects new public and internal Task submissions and leaves the new Task lifecycle owner and approval dispatcher stopped; `active` enables them. Roll every replica with `staged` before a separate deployment changes the shared value to `active`. |
| `STEWARD_KUBERNETES_TOKEN_REVIEW_AUDIENCE` | Required and non-empty. DEV uses `https://kubernetes.default.svc`. |
| `STEWARD_TASK_WORKFLOWS_JSON` | Required JSON array matching `workflows.example.json`. Commands are server-selected; clients cannot supply them. |
| `STEWARD_TASK_EXECUTION_BINDINGS_FILE` | Preferred read-only file containing `steward.execution-bindings/v1`. Missing or empty catalog means no coding agents are available. |
| `STEWARD_TASK_EXECUTION_BINDINGS_JSON` | Optional inline form of the same document for non-Helm integration environments. Configuring both forms fails startup. |
| `STEWARD_EXECUTION_ENABLED` | Set to `false` for core-only installation: Task orchestration and execution bindings must also remain `staged`. Set to `true` only after the governed prerequisites and staged rollout are ready. The chart's `execution.enabled` sets this for both binaries. |
| `STEWARD_TASK_INFERENCE_ENDPOINT` | Required only with governed execution. Exact OpenAI-compatible Responses API endpoint rendered by the configured Codex adapter; route it through the governed inference provider. |
| `STEWARD_TASK_MCP_GW_ENDPOINT` | Exact HTTP(S) streamable MCP endpoint. Required only when a versioned task resolves non-empty tool authority; otherwise the task fails before reservation or execution. |
| `STEWARD_APISERVER_BIND` | HTTPS listener, default `0.0.0.0:8443`. Expose the existing apiserver Service port to this target port. |
| Task API enablement | The Task routes exist when the apiserver starts, but core-only installation is staged and rejects new Task execution. Invalid required Task configuration still fails startup. |
| `STEWARD_DATABASE_URL` | Existing Postgres connection reference. Keep the value in a Secret, never this repository. |

The caller supplies `Authorization: Bearer <exchanged-token>`. GitHub requests the production
exchange service's audience; Steward never receives the raw GitHub OIDC token. Steward sends the
exchanged token to TokenReview with the configured Kubernetes API server audience. The exchanged
JWT has `aud=steward-task-api`, which the customer's cluster identity provider must be configured
to validate; Steward does not parse or reinterpret that JWT. DEV uses an EKS external OIDC client
ID with the same value. See `docs/task-submission-api.md` for the required verified
username/groups and the external mapper boundary.

For a tool-bearing versioned Workflow using the `codex-v1` adapter, Steward writes the configured
MCP endpoint and a bearer environment-variable name into the task's fresh adapter configuration.
The command sets that variable only to OpenShell's documented non-secret provider placeholder.
OpenShell derives the runtime bearer at the governed egress boundary; the plan, database, and
agent configuration never contain a provider credential. Tool-less plans contain no MCP server
entry or MCP bearer variable and do not require the endpoint.

Execution images, versions, executable paths, and OpenShell provider profiles are deployment
configuration, not values owned by this E2E fixture or by Steward production code. The complete
schema, Helm surface, released validator, empty-catalog behavior, lifecycle rules, and upgrade
procedure are documented in
[`docs/installation/execution-bindings.md`](../../docs/installation/execution-bindings.md).
The concrete agent and profile values in this directory are local E2E fixtures only.

## Controller inputs

With `STEWARD_EXECUTION_ENABLED=false`, the controller starts its database-backed admission
webhook without OpenShell, LiteLLM, Mint, or Jira. This is the fail-closed core-only mode: Task
orchestration must remain `staged`, and no Task worker or approval dispatcher runs. With governed
execution enabled, `STEWARD_OPENSHELL_ENDPOINT`, the LiteLLM URL and master key, workload identity,
and the other [installation prerequisites](../../docs/installation/installation-guide.md) are
required even for the model-free copy-smoke Workflow below. When orchestration becomes `active`,
the approval dispatcher uses Jira if configured; with Jira disabled, decisions requiring that
channel are rejected, not silently approved. `STEWARD_S0_BOOTSTRAP=1` is a bootstrap-only mode
and does not run the durable Task worker or approval dispatcher; do not use it for a Task
deployment. No second Task controller service port exists.

The chart's `config.taskOrchestrationMode` supplies the same required mode to both binaries and
defaults to `staged`. A migration rollout first deploys all new binaries with that default, verifies
that no legacy Task writer remains, and only then performs a separate Helm change to `active`.
Existing Task reads and exact idempotent retries remain available while staged, but no new Task
intent or approval delivery may begin.

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

The input and output names are workspace-relative tar paths. This smoke makes no LiteLLM or MCP
call, but governed controller startup still requires its configured LiteLLM and OpenShell planes.
Inject the JSON through a ConfigMap or equivalent configuration source as
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

The production identity contract for this credential is exact:

- exchanged JWT audience and the cluster identity provider's client ID: `steward-task-api`
- delegated Kubernetes TokenReview audience: the required
  `STEWARD_KUBERNETES_TOKEN_REVIEW_AUDIENCE`; DEV uses
  `https://kubernetes.default.svc`
- group, exactly once: `agents.apelogic.ai/service-envelope-bootstrap:steward-run`
- username: a non-empty identity verified by the cluster identity provider and preserved as the
  envelope's `authored_by` audit value

This identity can call only `POST /admin/service-envelopes/steward-run`. It is denied approvals,
grants, member envelopes, other service envelopes, and every other administrator route. Combining
the bootstrap group with the broad administrator group or a member-role group fails
authentication.

Bootstrap requires a Steward release containing both the route-scoped authorization contract and
the delegated Kubernetes TokenReview audience setting, plus a customer-operated identity provider
and short-lived exchange profile that can issue the exact verified identity above. DEV uses an
EKS OIDC association for this purpose; a customer cluster must supply and test its own equivalent.
Steward does not issue that token. It must not be stored in a Kubernetes Secret or replaced with
any other long-lived credential; the chart intentionally has no bootstrap-token Secret input.

## Optional Jira decision channel

The chart defaults to `jira.enabled=false`; neither binary mounts a Jira token or needs Jira
values in that mode. If a decision workflow uses Jira, enable it explicitly and supply all four
real inputs below. A partially configured Jira adapter fails startup; no dummy value is valid:

| Environment input | Helm value/source | Required value shape |
|---|---|---|
| `STEWARD_JIRA_BASE_URL` | `config.apiserver.jiraBaseUrl` | Public HTTPS Jira tenant root, without a REST API suffix |
| `STEWARD_JIRA_PROJECT_KEY` | `config.apiserver.jiraProjectKey` | Existing project key used for Steward decisions |
| `STEWARD_JIRA_ACCOUNT_EMAIL` | `config.apiserver.jiraAccountEmail` | Account email corresponding to the API token |
| `STEWARD_JIRA_TOKEN` | Secret named by `secrets.jira.name`, key `secrets.jira.key` | Raw Jira API token |

The tenant URL, project key, and account email are deployment inputs; the token is secret-store
material. None belongs in this public repository. Decisions needing Jira fail closed when the
channel is disabled.

## Release compatibility

Use apiserver, controller, and chart artifacts from the same exact Steward revision. Older
image/chart handoffs may not support the core-only execution flag, optional Jira projections, or
the current delegated TokenReview audience; do not mix them with this configuration.
