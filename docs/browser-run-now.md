# Browser Run now

Status: current implementation

Steward users can start a governed Task from the browser without first creating a
GitHub Actions workflow. The browser route is:

```http
POST /app/api/v1/runs
X-Steward-CSRF: <session proof>
Idempotency-Key: <unique request key>
Content-Type: application/json
```

The request selects one package locator and, when the user has multiple active
Envelopes, one exact Envelope digest. Actor, owner, and canonical user identity
always come from the authenticated browser session; request bodies cannot supply
them.

## Package sources

| Source | Revision accepted from the browser | Immutable evidence recorded by Steward |
|---|---|---|
| `inline` | Optional `steward:sha256:<digest>` | Server-computed closure digest and the exact inline files |
| HTTPS Git repository | `git:ref:<ref>` or `git:sha1:<commit>` | Exact resolved `git:sha1:<commit>` and closure digest |
| `steward:registry/<name>` | `steward:version:<n>` | The immutable published Workflow version and digest |

Git reads use the deployment's configured source adapter. Browser runs accept only a
repository whose stable owner and repository IDs appear as a `source` in the
operator-managed source-repository binding catalog. Steward enforces that allowlist
before reading package content. An unlisted repository and a repository that the
source adapter cannot resolve return the same authorization response, so this endpoint
does not reveal private repository existence. The adapter then authenticates the
repository, resolves symbolic refs before reading package files, and revalidates the
stable repository identity. The browser session remains the Task's acting-user
authority and audit identity; repository authentication does not grant runtime
authority.

Every source resolves to the same v2 TaskDefinition and package-closure rules. Inline
files are limited to 64 KiB in total. `inputs` must be a JSON object of at most 16 KiB
and is materialized as `in/inputs.json`; it cannot select an agent, model, tool, or
Envelope. The prefilled single-prompt package stores its prompt as `promptText` in the
TaskDefinition, so the exact inline package contains one file.

## Admission and lifecycle

Steward resolves the selected active User Envelope, evaluates the package's effective
requirements against it, then independently applies the existing service-Envelope and
execution-binding checks. A package outside the selected Envelope is rejected before a
Task or runtime effect is reserved.

At most four unfinished browser-origin Tasks may be active for one canonical user.
The limit is enforced transactionally, returns HTTP 429 for a new fifth Task, and
does not prevent an exact idempotent retry from completing an interrupted submission.

The Task record captures:

- browser origin and canonical acting user;
- exact User Envelope instance, revision, digest, and approved snapshot;
- exact package pin, closure, and closure digest; and
- the existing runtime plan and execution binding.

An idempotency retry recomputes and compares that evidence. It can finish input upload
or execution-request persistence after an interrupted submission, but changed package
bytes, a moved Git ref, changed inputs, or changed authority returns a conflict.

## Outputs

The run detail uses the existing browser run routes. A succeeded run exposes only
regular files below `out/`:

```text
GET /app/api/v1/runs/{taskUid}/outputs
GET /app/api/v1/runs/{taskUid}/outputs/{path}
```

Both routes are scoped to the authenticated canonical owner. Failed, incomplete,
cross-owner, malformed, and path-traversing archives are not downloadable. Execution
stdout and stderr remain available through the existing log routes.

A succeeded inline run also exposes its exact persisted package files to its owner:

```text
GET /app/api/v1/runs/{taskUid}/package
```

Failed, incomplete, non-inline, and cross-owner runs return not found. This prevents a
failed attempt or mutable browser form state from being presented as a known-good
repository package. The run detail renders either inline `promptText` or the resolved
prompt file in the same Task prompt panel and records the prompt source in Run detail.

## Template control

Each immutable Envelope-template revision records
`allowInlineBrowserTasks`. It defaults to `true`. Administrators can disable inline
authoring for a successor template revision without disabling repository packages or
published Workflow aliases.

The user **Run now** page offers a prefilled no-tool hello-world package, Git package
locators, and published Workflows. Inline authoring lists agents directly from the
deployment-owned execution-binding catalog; it does not require a published Workflow.
It selects only a coding agent whose model family is allowed by the selected Envelope
and records that exact model in the package requirements.

After an inline run succeeds, its detail page offers **Save this task to a repository**.
**Copy repository bundle** reads the exact persisted package from the successful run,
then asks the server to render the handoff against the same provisioned Envelope and
the deployment's reviewed `steward-run` release. With `steward-run` v0.8.0 or later,
the copied JSON file map contains the unchanged `task-definition.json` with its inline
prompt and a caller workflow using `package-path`. Older reviewed releases receive the
compatible invocation manifest and `invocation-path` workflow instead. Either bundle
uses the exact reusable-workflow commit and preserves the package closure digest proven
by the inline run; release coordinates are never guessed by the browser client.

## First-run prerequisite failures

The 0.3.8 first-run path fails before creating a Task or runtime when a required
deployment or identity prerequisite is absent. Use the following exact signals; do
not diagnose these cases as generic database, Kubernetes, or credential failures.

| Broken prerequisite | Authoritative signal | Operator action |
|---|---|---|
| Task orchestration is `staged` | **Admin → Get started** reports `Task orchestration is staged.` A run submission returns HTTP 503 with `TaskRuntimeContractUnavailable("Task submission is disabled during the staged orchestration rollout")`. | Complete the documented staged rollout and activate Task orchestration. |
| `githubSource` is disabled | Direct-package submission returns HTTP 503 with `error: task.direct_package_source_disabled` and `failureReason: Direct package source resolution is disabled on this Steward deployment; enable githubSource to accept in-repository Task packages.` | Configure and enable the exact Git source adapter before retrying. Browser-inline packages remain available. |
| No published template grants a member role | **Admin → Get started** reports `No template grants member roles, models, and tools together.` The affected member sees no eligible template to request. | Publish a successor template revision with at least one eligible member role and the intended model and tool authority. |
| The GitHub Actions actor is unassociated and connection-based auto-association is unavailable or disabled | The first valid v3 submission returns HTTP 403 with `error: task_identity_unassociated`, the verified `issuer` and `subject`, and `message: The authenticated federated subject is not associated with a Steward user. Ask a Steward administrator to associate it.` | Associate the observed subject with the existing Steward member, or enable connection-based auto-association and verify the member's GitHub connection. |

All four failures are fail-closed. The staged and disabled-source checks run before
Task reservation; the unassociated subject receives no Task authority; and a
role-less template is not offered to a member.
