# Outputs

A run's results are the files the agent writes under `out/` in its output directory. The path is
in the `STEWARD_OUTPUT_DIR` environment variable, so tell the agent to write to
`$STEWARD_OUTPUT_DIR/out/`. Nothing else leaves the sandbox: not the workspace checkouts, not
scratch space, not a writable `copy` of a repository.

## Declare what the Task produces

A Task definition lists its outputs. Each has a `path` that starts with `out`, a `kind` of `file`
or `directory`, and `required`:

<!-- schema: taskDefinition -->
```json
{
  "schemaVersion": "steward.task-definition/v2",
  "name": "release-summary",
  "version": 1,
  "runtime": { "agentRef": "example-agent@1.2.3" },
  "promptText": "Summarize the changes since the last tag. Write out/summary.md and put any charts in out/charts/.\n",
  "workspace": [{ "git": { "repository": "self", "history": { "depth": 200 } } }],
  "outputs": [
    { "path": "out/summary.md", "kind": "file", "required": true },
    { "path": "out/charts", "kind": "directory", "required": false }
  ]
}
```

A Task needs at least one output and can list up to 32. To collect everything under `out/`,
declare `{ "path": "out", "kind": "directory", "required": true }`.

## When a run produces nothing

A run that finishes without writing any file under `out/` fails with "This run finished without
creating an output file." Ask the agent explicitly to write its result there; the browser
reminds you of this next to the prompt.

## Get the files

- **Browser:** the run's detail page lists the outputs under **Outputs**, with download links.
- **GitHub Actions:** list the paths in the action's `outputs` input, and they are copied back
  into the job's workspace when the run succeeds.
- **Jenkins, Temporal and other API callers:** download the output archive once the run reaches
  a terminal phase. Asking earlier returns a "not ready" response. See
  [Calling Steward](calling-steward.md).

Outputs are limited to 64 MiB per run.
