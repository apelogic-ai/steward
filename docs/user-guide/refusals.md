# Common refusals and what to do

Steward refuses a run before it starts when the request doesn't fit your Envelope or the
installation. A refused run reserves nothing and spends nothing. A run that fails later shows its
reason on the run page and in the API's `failureReason`.

The messages below are the sentences you see. API callers get them in `failureReason`, usually
with HTTP `422` and the code `task.admission_rejected`. Match on the HTTP status and code, not on
the exact wording.

## Envelope

| You see | Why | What to do |
|---|---|---|
| the authenticated user has no active provisioned User Envelope | You have no approved Envelope, or it expired. | Request one on the **Envelopes** page. |
| multiple active provisioned User Envelopes are ambiguous (`409`) | You have several active Envelopes and didn't choose one. | Choose one in the browser, or pass `envelopeDigest` (`envelope-digest` in GitHub Actions). |
| the selected Envelope is not active | The digest you passed isn't one of your active Envelopes. | Copy the current digest from the **Envelopes** page. If it expired, request it again. |
| Task submission and invocation manifest select different Envelopes | The request and the Task package name different Envelope digests. | Make them match, or remove one. |

## Requirements beyond your Envelope

Each message names the value that doesn't fit and the limit, for example "Monthly budget 80.00
USD is more than the 50.00 USD this Envelope allows". When several values don't fit, the
message lists them all, separated by semicolons.

| You see | What to do |
|---|---|
| Model … is not one of the models this Envelope allows | Choose an allowed model, or request an Envelope with that model. |
| direct TaskDefinition must select a runtime model when multiple models are available | Set `runtime.model` in the Task. |
| This coding agent does not support the selected model. … | Choose a model from the list the message gives, or another agent. |
| Tool … is not one of the tools this Envelope allows | Remove the tool from `requires`, or request an Envelope that includes it. |
| Monthly budget … / Per-run budget … is more than the … this Envelope allows | Lower the budget in `requires`. |
| A per-run budget with no limit is more than the … this Envelope allows | Set `singleRunLimit` to a value instead of `null`. |
| Maximum run duration … is more than the … this Envelope allows | Shorten `ttl`. |
| Runtime allowance … min is more than the … min per month this Envelope allows | Lower `runtimeMinutesLimit`. |
| Runner platform … / Runner memory: requested …, limit … | Lower the runner request, or use `null` for no specific size. |
| Workspace access: requested …, but this Envelope does not allow workspace access | Request an Envelope with workspace access, or remove `workspace`. |

## Repositories and workspaces

| You see | Why | What to do |
|---|---|---|
| The requested workspace repository is unavailable. … | The repository isn't on the installation's approved list, or the name is wrong. | Check the name; ask an administrator to approve it. |
| `workspace_git_repository_not_admitted` | Same, for a repository given by its IDs. | Same. |
| Workspace repository links must use the configured source host … | The link points to another host. | Use `owner/name`, or a link on the configured host. |
| The branch or tag … was not found in … | The `ref` doesn't exist. | Fix the branch or tag name. |
| `workspace_git_self_unavailable` | `self` was used in a run that didn't start from a repository, such as a browser run. | Name the repository instead of `self`. |
| Git history depth … is more than the … generations this Envelope allows. | `history` asks for too many commits. | Ask for a smaller depth. |
| Git submodule depth … is more than the … levels this Envelope allows. | Submodules aren't allowed, or recursion goes too deep. | Turn submodules off, or set `recursive` to `false`. |
| `workspace_git_required_submodule_unavailable` | A submodule in `required` couldn't be checked out. | Approve its repository, or remove it from `required`. |
| Workspace materialized size … / Workspace file count … is more than … | The checkout is too big. | Use `paths` to check out less, or ask for higher limits. |
| `workspace_git_name_requires_explicit_valid_name`, `workspace_entry_name_conflict` | The default directory name is empty or used twice. | Set `name` on the entry. |
| The repository is not authorized as a browser Task source. … (`403`) | The browser can't run Task packages from this repository. | Ask an administrator to allow it. |

## Sign-in and requests

| You see | Why | What to do |
|---|---|---|
| `401` | The token is missing, expired or invalid. | Get a fresh token. API callers exchange a new one for each request. |
| The credential is valid, but its canonical user is not registered in Steward. … (`403`) | Your account hasn't been set up in Steward. | Ask an administrator to onboard you. |
| idempotency key is already bound to another task request (`409`) | You reused an `Idempotency-Key` with a different request. | Use a new key for a new request. Reuse a key only to retry the exact same request. |
| too many active browser-authored tasks (`429`) | You already have four browser runs in progress. | Wait for one to finish. |
| Add an inference key under Connections > Inference / LLMs, then retry the run. | The installation uses your own model key, and you haven't added one. | Add it on the **Connections** page. |

## When a run fails after it starts

| You see | What to do |
|---|---|
| This run finished without creating an output file. | Make the prompt write at least one file under `$STEWARD_OUTPUT_DIR/out/`. See [Outputs](outputs.md). |
| This run could not start because the monthly spending limit of … has been reached. | Wait for next month, or ask for a higher monthly limit. |
| This run reached its spending limit of …. | Raise the per-run limit, within your Envelope. |
| This run reached its limit of … minutes. | Raise the limit, or split the work. |
| The model service rejected your saved API key. … | Replace the key on the **Connections** page. |
| The saved API key has no remaining budget. … | Replace the key, or ask an administrator to raise its budget. |
| The workspace could not be prepared. … | Retry. If it happens again, tell an administrator. |
| This run could not start its coding agent. … | Retry. If it happens again, tell an administrator. |

If a message isn't listed here, the run page and the execution log usually say more. Give your
administrator the run's Task ID when you ask for help.
