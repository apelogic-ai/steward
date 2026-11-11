# Read a run

Open **Runs** and select a run. The detail page answers three questions: what was approved, what
happened, and what came out.

## Status

| Status | Meaning |
|---|---|
| `submitted`, `queued` | Steward accepted the request and is preparing the run. |
| `running` | The agent is working. |
| `succeeded` | The run finished and produced its outputs. |
| `failed` | The run stopped. The page shows the reason in plain words. |
| `cancelled` | Someone cancelled the run. |

You can cancel a run that hasn't finished.

## Stages

The timeline shows how long each stage took:

- **Admission:** checking the request against your Envelope and pinning repositories.
- **Runtime:** creating the sandbox.
- **Agent:** the coding agent working.
- **Finalize:** collecting outputs and removing the sandbox.

A run that fails in admission never started anything, and nothing was spent.

## What was approved

The page records exactly what the run was allowed:

- the **Envelope** and its digest;
- the exact **Task** version and package digest;
- the **workspace**: every repository with its exact commit, history, selected paths, and each
  submodule as checked out or skipped with a reason (see
  [Submodules](repositories-and-workspaces.md#submodules)), plus a content digest for the whole
  workspace.

These records don't change after the run, so you can always see what it used.

## Log

The execution log shows what the agent did. Read it first when a run fails or produces the wrong
result.

## Spend and tokens

Run views show the run's **Spend** and **Tokens used** when the model gateway reports them. When
it doesn't, they show **Not reported**. Administrators also see why on the Agent Runs pages:

| Reason | Meaning |
|---|---|
| Spend reporting isn't available yet when runs use people's own model keys. | The installation uses your own inference key, and spend for those runs isn't reported yet. |
| The model gateway hasn't reported spend for this run. | No usage has been recorded for the run, for example because it failed before calling a model. |
| The model gateway reported spend but not tokens used. | Spend is known; token counts aren't. |

Not reported doesn't mean the budget isn't enforced. Budget limits apply either way.

## Outputs

Successful runs list their output files under **Outputs** with download links. See
[Outputs](outputs.md). If a run failed, see [Common refusals](refusals.md).
