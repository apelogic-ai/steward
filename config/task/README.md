# Task production configuration

The Task API is part of `steward-apiserver`; the durable Task worker is part of
`steward-controller`. Steward v0.3 supports direct Git packages and immutable versioned
Workflows. The removed unversioned workflow catalog is not a supported execution path.

## Authority

Every externally submitted Task requires an active, provisioned User Envelope owned by the
authenticated canonical user. When several are active, the request selects one by its exact,
owner-scoped `envelopeDigest`; omitting the selector is supported only when exactly one is active.
The v0.2-compatible path still requires exactly one active, provisioned User Envelope.
Steward validates the requested runtime against the selected approved snapshot, then persists the
Envelope instance ID, revision, digest, snapshot, effective requirements, and admission result.
The controller recovers only that immutable evidence.

The deployment capability catalog is descriptive. It supplies model and tool choices to the
administrator template editor and grants no Task authority. Product-owned Connection operations
use fixed, versioned internal authorities compiled into Steward; those authorities cannot admit a
user Task.

## Apiserver inputs

| Input | Supported production value |
|---|---|
| `STEWARD_TASK_ORCHESTRATION_MODE` | Required in both binaries. `staged` rejects new public and internal Task submissions; `active` enables orchestration. Roll every replica with `staged` before a separate activation change. |
| `STEWARD_KUBERNETES_TOKEN_REVIEW_AUDIENCE` | Required and non-empty. |
| `STEWARD_TASK_EXECUTION_BINDINGS_FILE` | Preferred read-only `steward.execution-bindings/v2` catalog. Missing or empty means no coding agents are available. |
| `STEWARD_TASK_EXECUTION_BINDINGS_JSON` | Optional inline equivalent for non-Helm integration environments. Configuring both forms fails startup. |
| `STEWARD_CAPABILITY_CATALOG_FILE` | Read-only `steward.capability-catalog/v2` document used by browser administration. Required when browser administration is enabled. |
| `STEWARD_CAPABILITY_CATALOG_JSON` | Optional inline equivalent for non-Helm integration environments. Configuring both forms fails startup. |
| `STEWARD_CUSTOM_ENVELOPE_SAFETY_CEILING_FILE` | Deprecated compatibility input. It is accepted but ignored, emits a startup warning, and will be removed in the next release. |
| `STEWARD_CUSTOM_ENVELOPE_SAFETY_CEILING_JSON` | Deprecated inline compatibility input with the same ignored-and-warn behavior. |
| `STEWARD_RUN_RELEASE_FILE` | Preferred read-only normalized steward-run release handoff for browser Workflow generation. Browser administration requires exactly one file or inline form and steward-run v0.7.0 or later. |
| `STEWARD_RUN_RELEASE_JSON` | Inline equivalent used by the Helm chart. Configuring both release-handoff forms fails startup. |
| `STEWARD_RUN_WORKFLOW_INSTALLATION_MODE` | Optional `remote` (default) or `vendored` caller rendering mode. Vendored mode requires steward-run v0.7.6 or later and its verified release asset at `.github/workflows/steward-task-vendored.yml` in the caller repository. |
| `STEWARD_DEFAULT_LLM_TEMPLATE_JSON` | Optional ordinary catalog seed for the disabled-by-default LLM smoke template. Its model, roles, budget, TTL, revision, and empty tool set are explicit non-secret configuration. |
| `STEWARD_EXECUTION_ENABLED` | `false` for core-only installation with orchestration staged; `true` after governed dependencies and execution bindings are ready. |
| `STEWARD_TASK_INFERENCE_ENDPOINT` | Required with governed execution. Exact OpenAI-compatible Responses API endpoint used by the Codex adapter. |
| `STEWARD_TASK_MCP_GW_ENDPOINT` | Exact HTTP(S) streamable MCP endpoint, required when effective Task authority contains tools. |
| `STEWARD_DATABASE_URL` | PostgreSQL connection held in a Secret, never in this repository. |

The caller supplies an exchanged bearer identity, never a raw GitHub OIDC token. Steward verifies
the configured identity contract and binds the Task to the canonical user before selecting that
user's active provisioned Envelope. Source provenance for a direct package is verified and pinned
independently of authority.

## Capability catalog

The Helm value `config.apiserver.capabilityCatalog` is schema-validated and mounted through an
immutable checksum-named ConfigMap. It contains model and tool identities plus the provider
catalogs shown on the Connections page:

```yaml
config:
  apiserver:
    capabilityCatalog:
      schemaVersion: steward.capability-catalog/v2
      models:
        - provider: openai
          model: gpt-5.4
      tools:
        - provider: github
          resource: actions_get
          action: read
          accessClass: read
        - provider: cli
          resource: kubectl
          action: read
          accessClass: read
        - provider: cli
          resource: kubectl
          action: write
          accessClass: write
        - provider: cli
          resource: kubectl
          action: destructive
          accessClass: destructive
        - provider: remote-shell
          resource: remote_shell_run
          action: write
          accessClass: write
      catalogs:
        - provider: github
          catalogId: github-tools
          version: 1.6.0
          available: true
```

Template-free requests always require administrator approval. Requests above a template's limits
also require approval and must include a justification. Steward checks the reviewed request's
model, tool, and workspace capabilities when it is created and checks them again immediately
before approval. A request inside its template limits provisions without using the descriptive
capability catalog as an additional authority boundary.

Access classes and provider availability are descriptive UI metadata. It contains no budget,
TTL, user, template, Envelope, or admission fields. Changing it changes
what the editor can offer; it does not change an existing template, User Envelope, Task, or
admission decision.

The governed CLI and remote-shell tuples above are the exact grants checked by MCP-GW. List only
the command packs and classes installed in that deployment. See
[Governed CLI and remote shell](../../docs/installation/governed-cli-remote-shell.md) for Task,
Envelope, and machine run-binding configuration.

Steward reads connection status once for every distinct provider named by `catalogs`, using that
exact provider identifier with the gateway's stable version-2 status route. Multiple catalog
entries for one provider produce one Connections entry, and the entry is enabled when any of its
catalogs is available. A status failure marks only that provider unavailable; it does not hide the
other configured providers. Provider credentials remain in the gateway.

## Controller inputs

With execution disabled, the controller starts its database-backed admission webhook without
OpenShell, LiteLLM, Mint, or Jira, and orchestration must remain `staged`. With execution enabled,
configure the OpenShell endpoint, inference plane, workload identity, execution bindings, and any
tool provider profiles used by Tasks. Jira remains optional and fail-closed when a required
decision channel is unavailable.

## Database and rollout

Both production binaries run the append-only SQL migrator at startup. An upgrade from released
v0.2.6 applies migrations `0040` through `0051`; the release head is `0051`. They preserve existing
Task, request, approval, identity, and Envelope history while adding the v0.3 catalog, browser,
observability, escalation, and custom-request state. See
[the migration register](../../migrations/README.md) and the
[v0.3 upgrade guide](../../docs/installation/upgrade-v0.3.0.md) for the mixed-version and rollback
boundaries.

Use apiserver, controller, web, chart, and supporting images from the same exact release. Stop
template authoring during the v0.2.6-to-v0.3.0 rolling upgrade, and do not return pre-v0.3 writers
after template-free requests, multiple active Envelopes, or new catalog-only template revisions
exist.

## Optional Jira decision channel

The chart defaults to `jira.enabled=false`. If a decision workflow uses Jira, enable it explicitly
and supply the HTTPS tenant URL, project key, account email, and token Secret. A partial Jira
configuration fails startup; no placeholder value is valid.
