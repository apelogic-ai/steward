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

The caller supplies `Authorization: Bearer <assertion>`. For `steward-run`, the GitHub Actions
OIDC token must use audience `steward-task-api`. See `docs/task-submission-api.md` for the
required TokenReview mapper and the currently open production identity blocker.

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

`workflows.example.json` is a complete workflow-catalog value. Inject it through a ConfigMap
or equivalent configuration source as `STEWARD_TASK_WORKFLOWS_JSON`; do not let submitters
override its command, namespace, models, tools, budget, or TTL.

Before enabling `steward-run`, an administrator must author a service envelope for the exact
service name `steward-run`. `steward-run-service-envelope.example.json` is the matching example
and can be submitted to:

```text
POST /admin/service-envelopes/steward-run
```

If the workflow exceeds that envelope, submission returns `202` and parks. If no service
envelope exists, submission fails closed.
