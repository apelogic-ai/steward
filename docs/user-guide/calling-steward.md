# Start runs from other tools

Besides the browser, four kinds of callers can start runs. In every case Steward applies the
same Envelope checks; the caller can't choose a wider Envelope than the person or service it acts
for.

| Caller | Who it runs as | Set up by | Details |
|---|---|---|---|
| GitHub Actions | The person who triggered the workflow | You, with a generated workflow file | [GitHub Actions generator](../github-actions-generator.md) |
| Jira | A service owner the administrator configured | An administrator | [Jira webhook Triggers](../calling-steward-from-jira.md) |
| Jenkins | A service identity | Your CI administrator | [Jenkins and Temporal](../calling-steward-from-jenkins-and-temporal.md) |
| Temporal | A service identity | Your platform team | [Jenkins and Temporal](../calling-steward-from-jenkins-and-temporal.md) |

## GitHub Actions

The easiest start is in the browser: run a Task inline, then on the finished run choose
**Publish this task to GitHub**. Steward opens a pull request that adds two files: the Task
definition and a workflow that calls it. After you merge it, **Run on GitHub** starts it, and
the run detail page shows the GitHub run next to the Steward run.

You can also write the workflow yourself. It uses the `steward-run` action with one of:

- `package-path`: a Task definition in the same repository, for example
  `.steward/tasks/release-summary/task-definition.json`;
- `invocation-path`: an invocation manifest that points at a Task package in another approved
  repository;
- `workflow`: a Task published in Steward, as `name@N`.

Other inputs you are likely to use:

| Input | Use |
|---|---|
| `envelope-digest` | Choose one Envelope when you have several. |
| `inputs` | Files from the job workspace to send to the run, one path per line. |
| `outputs` | Paths under `out/` to copy back into the job workspace, one per line. |
| `idempotency-key` | Your own retry key. Leave it empty and the action derives one from the job attempt and the Task. |

The action sets outputs that later steps can read: `outcome`, `status`, `task-uid`, and, on a
refusal, `error-code`. A GitHub Actions run gives the Task's `self` repository the exact commit
the workflow ran on; see [What `trigger` means](repositories-and-workspaces.md#what-trigger-means).

Inside the workflow, the request body Steward receives looks like this:

<!-- schema: directTaskSubmission -->
```json
{
  "contractVersion": "steward.task/v2",
  "packagePath": ".steward/tasks/release-summary/task-definition.json",
  "diagnostics": { "executionLog": "full" }
}
```

## Jira

An administrator connects a Jira webhook to a published Task. When a matching issue event
arrives, Steward starts one run as the configured service owner, under that owner's Envelope.
Jira Triggers allow read-only tools only, and Steward never writes back to Jira. The run shows a
link to the issue when the administrator configured the Jira site.

## Jenkins and Temporal

These callers use the Run API directly:

1. Exchange the job's credential for a short-lived Steward token.
2. Create the run with `POST /v1/tasks` and a stable `Idempotency-Key`.
3. Upload inputs as a tar archive, then start the run.
4. Poll its status until it is `succeeded`, `failed` or `cancelled`.
5. Download outputs, read `failureReason` if it failed, and clean up.

Pick the idempotency key from something that stays the same across retries of the same job, for
example `jenkins:<job>:<build-number>`, and never add a timestamp or attempt number. Reusing the
key with a different request is refused with `409`.

To show which build started a run, add `callerReference` with a label and an HTTPS link. Run
views then show it.
