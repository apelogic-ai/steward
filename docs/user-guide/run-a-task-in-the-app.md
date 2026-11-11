# Run a Task from the app

You can start a run from the browser without writing a workflow. You need an active Envelope;
if you don't have one, request it on the **Envelopes** page first (see
[Concepts](concepts.md#envelope)).

## Run now

Open **Run now** and choose where the Task comes from.

**Inline prompt.** The quickest way to try something:

1. Choose a **Coding agent** and a **Model**. The lists show only models your Envelope allows
   that the agent supports.
2. Write the **Prompt**. Tell the agent to write its results under `$STEWARD_OUTPUT_DIR/out/`.
   Only files there are collected, and a run that writes no file there fails.
3. Optionally tick **Check out this repository**:
   - Choose a **Workspace repository** from the approved list. The first choice, "Default
     repository at trigger revision", checks out that repository's default-branch HEAD, because
     a browser run has no starting commit.
   - Choose a **Branch or tag**. It starts as the repository's default branch.
   - Optionally set a **History depth** for offline `git log` and `git blame`. Leave it empty to
     get files only.
4. Start the run.

Without requirements, an inline run may use everything your Envelope allows. Your administrator
can turn inline prompts off for a template; Git packages and published Tasks still work then.

**Git repository.** Runs a Task definition that is committed to a repository:

1. Enter the **Repository**. It must be on the installation's approved list.
2. Enter a **Branch, tag, or exact commit**. Steward resolves a branch or tag to one exact
   commit before it starts.
3. Enter the **Package path**, for example `.steward/tasks/release-summary/task-definition.json`.

**Published Task.** Runs a Task that someone published in Steward, at an exact version.

If you have more than one active Envelope, choose which one to use. You can have up to four
browser runs in progress at once.

## Save a Task with requirements

To reuse a Task, create it under **New Task**. The form has a **Requirements (optional JSON)**
field:

- Leave it empty to let the Task use everything your Envelope allows.
- Fill it in to limit the Task to less. It takes the same object as `requires` in a Task
  definition, and it must be complete:

<!-- schema: directRequirements -->
```json
{
  "authority": {
    "llms": [{ "provider": "litellm", "model": "review-model" }],
    "tools": [],
    "budget": { "monthlyLimit": "20.00", "singleRunLimit": "1.00", "currency": "USD" },
    "ttl": "30m",
    "runner": { "platforms": ["linux"], "memory": null, "compute": null, "storage": null }
  }
}
```

Here `"tools": []` means the run gets no tools, and `null` for a runner size means "no specific
request". Every value must fit inside your Envelope. The field list is in the
[reference](reference.md#requirements).

You can save a draft Task before you have an active Envelope. You need one to run it.

## After the run

Open the run from **Runs** to follow its stages, read the log and download outputs. See
[Read a run](reading-a-run.md). From a completed inline run you can open the exact Task again,
rerun it, or publish it to a repository.
