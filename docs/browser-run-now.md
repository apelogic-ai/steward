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

After an inline run succeeds, its detail page offers **Publish this task to GitHub**.
The browser sends a blank repository query to `GET /app/api/v1/github/repositories`.
What that lists depends on the deployment (unreleased; after 0.3.12):

- **`githubSource` enabled with at least one binding.** Steward lists the admitted
  source repositories, the distinct sources in `githubSource.bindings`, including
  organization-owned ones. The apiserver resolves each bound repository ID through the
  source GitHub App, with a token scoped to that repository and only `metadata: read`.
  No governed operation or sandbox runs, and the response has `source: "admitted"` and
  an empty `login`. Every entry is `ready: true`. The result is cached in process for
  10 minutes. After that the cached listing is still served at once while one
  background refresh revalidates it. A refresh that fails keeps a previous entry for
  up to 20 minutes since it last resolved, counting it as unresolved; a definitive
  rejection, such as the App no longer being installed on it, removes it at once.
  If the App resolves only some repositories, the response lists those, and the
  `x-steward-unresolved-repositories` header counts the rest; only the unresolved ones
  are retried, at most every 30 seconds. Only when no listing has ever resolved does
  the request fail, with HTTP 503 and `reason: "source_app_unavailable"`; requests in
  the next 5 seconds share that result. It does not fall back to the per-user listing,
  except when the configured hosting plane cannot describe repositories at all. The
  source App installation must include each admitted repository.
  This listing does not need or reflect the user's GitHub connection, and the user's own
  repositories that are not admitted no longer appear in it.
- **`githubSource` disabled, or no bindings.** Steward lists the repositories owned by
  the GitHub user of the governed connection (`source: "connection"`), as in 0.3.12.
  Repositories owned by an organization are not listed by default.

An explicit search `query`, such as `org:<organization>` or `repo:<owner>/<name>`,
always uses the governed per-user listing, which marks each repository as ready or not
ready. Readiness requires the repository's stable owner and repository IDs to be
admitted as a source in `githubSource.bindings`; a mutable repository name is not
authority. Organization policy may also have to allow the Steward OAuth App to access
the repository before the governed listing or publication can see it.

The admitted listing shows any authenticated browser user the names, default branches,
visibility and URLs of the repositories the operator configured as governed sources.
It grants nothing: publication, workflow detection, dispatch and run status below still
run through the user's governed connection.

Get started step 4 offers the same list as the task page's publish panel, labelling a
repository that is not admitted as a governed source **Not admitted** (only the
per-user listing can contain one). The browser loads the list once per page load and
shares it between Get started and this panel; **Refresh repositories** in Get started,
**Retry repositories** after a failure, or a page reload fetches it again.

Publication is a server-owned operation. Steward reconstructs the successful run's
exact one-file inline package, renders the pinned `steward-run` v0.8.0-or-later caller,
creates a `steward/task-<taskUid>` branch from the repository's default branch, pushes
exactly those two files, and opens a pull request. It never writes to the default
branch. The response reports the package closure digest proven by the browser run.
Retries for the same Task and repository reuse the same durable operation identity, so
a changed browser retry key cannot create a second pull request.

After the pull request is merged, Steward verifies the exact generated workflow on the
default branch before enabling **Run on GitHub**. Dispatch is limited to that one
generated workflow, its declared `workflow_dispatch` inputs, and the default branch.
The run detail then shows the GitHub run, jobs, bounded failed-job log content, and the
owner-scoped governed Task correlated by repository, run ID, and attempt.

The browser routes are under `/app/api/v1` and remain bound to the authenticated
canonical owner. Per-user repository listings and lookups, workflow detection,
publication writes, dispatch, and run observation execute as audited governed
Connections operations; only the admitted repository listing above uses the source
GitHub App instead. No provider token or repository file content is logged. Migration 0064 adds those operation kinds and internal
authority v4 while preserving the exact v1, v2, and v3 authority tuples for historical
rows.

## First-run prerequisite failures

The 0.3.12 first-run path fails before creating a Task or runtime when a required
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
