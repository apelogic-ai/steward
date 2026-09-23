# Task production configuration

The Task API is part of `steward-apiserver`; the durable Task worker is part of
`steward-controller`. Steward v0.2 supports direct Git packages and immutable versioned
Workflows. The removed unversioned workflow catalog is not a supported execution path.

## Authority

Every externally submitted Task requires exactly one active, provisioned User Envelope owned by
the authenticated canonical user. Steward validates the requested runtime against its exact
approved snapshot, then persists the Envelope instance ID, revision, digest, snapshot, effective
requirements, and admission result. The controller recovers only that immutable evidence.

The deployment capability catalog is descriptive. It supplies model and tool choices to the
administrator template editor and grants no Task authority. Product-owned Connection operations
use fixed, versioned internal authorities compiled into Steward; those authorities cannot admit a
user Task.

## Apiserver inputs

| Input | Supported production value |
|---|---|
| `STEWARD_TASK_ORCHESTRATION_MODE` | Required in both binaries. `staged` rejects new public and internal Task submissions; `active` enables orchestration. Roll every replica with `staged` before a separate activation change. |
| `STEWARD_KUBERNETES_TOKEN_REVIEW_AUDIENCE` | Required and non-empty. |
| `STEWARD_TASK_EXECUTION_BINDINGS_FILE` | Preferred read-only `steward.execution-bindings/v1` catalog. Missing or empty means no coding agents are available. |
| `STEWARD_TASK_EXECUTION_BINDINGS_JSON` | Optional inline equivalent for non-Helm integration environments. Configuring both forms fails startup. |
| `STEWARD_CAPABILITY_CATALOG_FILE` | Read-only `steward.capability-catalog/v1` document used by browser administration. Required when browser administration is enabled. |
| `STEWARD_CAPABILITY_CATALOG_JSON` | Optional inline equivalent for non-Helm integration environments. Configuring both forms fails startup. |
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
immutable checksum-named ConfigMap. It contains only model and tool identities:

```yaml
config:
  apiserver:
    capabilityCatalog:
      schemaVersion: steward.capability-catalog/v1
      models:
        - provider: openai
          model: gpt-5.4
      tools:
        - provider: github
          resource: actions_get
          action: read
```

It contains no budget, TTL, user, template, Envelope, or admission fields. Changing it changes
what the editor can offer; it does not change an existing template, User Envelope, Task, or
admission decision.

## Controller inputs

With execution disabled, the controller starts its database-backed admission webhook without
OpenShell, LiteLLM, Mint, or Jira, and orchestration must remain `staged`. With execution enabled,
configure the OpenShell endpoint, inference plane, workload identity, execution bindings, and any
tool provider profiles used by Tasks. Jira remains optional and fail-closed when a required
decision channel is unavailable.

## Database and rollout

Both production binaries run the append-only SQL migrator at startup. Steward v0.2 migration
`0039_user_envelope_only_task_authority.sql` rejects an unfinished v0.1.23 Task unless it has one
complete User Envelope pin or one complete internal-authority pin. Terminal historical rows remain
readable. See [the migration register](../../migrations/README.md) and the installation guide for
upgrade and rollback boundaries.

Use apiserver, controller, web, chart, and supporting images from the same exact release. Do not
mix v0.1.23 writers with a database that has admitted v0.2 User-Envelope-only Tasks.

## Optional Jira decision channel

The chart defaults to `jira.enabled=false`. If a decision workflow uses Jira, enable it explicitly
and supply the HTTPS tenant URL, project key, account email, and token Secret. A partial Jira
configuration fails startup; no placeholder value is valid.
