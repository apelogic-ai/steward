# Check out repositories

A Task can ask for Git repositories and empty scratch directories before the agent starts.
You list them in the Task's `workspace` array. Steward checks each entry against your
Envelope and the installation's limits, pins every repository to one exact commit, and copies
the files into the sandbox. The sandbox never receives a Git credential.

This page answers:

- [Where the checkout goes and what it is called](#where-the-checkout-goes)
- [Which repository is used](#choose-the-repository)
- [Which commit is used: every `ref` form](#choose-the-commit-ref)
- [What `trigger` means for other repositories](#what-trigger-means)
- [Changing the ref for one run](#changing-the-ref-for-one-run)
- [Git history](#git-history)
- [Checking out only some paths](#check-out-only-some-paths)
- [Submodules](#submodules)
- [Read-only or writable](#read-only-or-writable)
- [Scratch space and limits](#scratch-space-and-limits)

## A first example

This entry checks out the `main` branch of `example-org/service-api`:

<!-- schema: workspaceEntry -->
```json
{ "git": { "repository": "example-org/service-api", "ref": "main" } }
```

Put entries in the Task definition's `workspace` array:

<!-- schema: taskDefinition -->
```json
{
  "schemaVersion": "steward.task-definition/v2",
  "name": "api-review",
  "version": 1,
  "runtime": { "agentRef": "example-agent@1.2.3" },
  "promptText": "Review the API in the service-api checkout and write out/review.md.\n",
  "workspace": [
    { "git": { "repository": "self" } },
    { "git": { "repository": "example-org/service-api", "ref": "main", "history": { "depth": 50 } } },
    { "scratch": { "size": "1Gi" } }
  ],
  "outputs": [{ "path": "out/review.md", "kind": "file", "required": true }]
}
```

Every field except `repository` has a default, so `{ "git": {} }` is the repository that started
the run, at the commit that started it.

## Where the checkout goes

Each entry is copied to `/sandbox/workspace/<name>` before the agent starts.

- **Default name for a Git entry:** the repository's name without the owner, made lowercase.
  Each run of characters other than letters and digits becomes one `-`. Dashes at the start and
  end are dropped, and the result is cut to 40 characters.

  | Repository | Default name | Checkout directory |
  |---|---|---|
  | `example-org/Service-API` | `service-api` | `/sandbox/workspace/service-api` |
  | `example-org/my_repo` | `my-repo` | `/sandbox/workspace/my-repo` |
  | `example-org/repo.example.com` | `repo-example-com` | `/sandbox/workspace/repo-example-com` |

- **Default name for a scratch entry:** `scratch`.
- **Your own name:** set `name` to 1 to 40 lowercase letters, digits or `-`:

  <!-- schema: workspaceEntry -->
  ```json
  { "git": { "repository": "example-org/service-api", "name": "api" } }
  ```

- Two entries can't have the same name, including two default names that come out the same.
  The run is refused with `workspace_git_name_requires_explicit_valid_name` when a repository
  name has no letters or digits, and with `workspace_entry_name_conflict` when two names
  collide. Set `name` to fix either.

The agent's working directory is still the Task's input directory, not the workspace. Before
the agent starts, its prompt lists every checkout with its exact directory and whether it is
read-only. The full inventory, with each exact commit, is also in
`/sandbox/workspace/.steward-workspace.json`.

## Choose the repository

| `repository` value | Meaning |
|---|---|
| omitted or `"self"` | The repository that started the run. |
| `"example-org/service-api"` | A repository by its `owner/name`. Upper and lower case don't matter. |
| `"https://github.example.com/example-org/service-api"` | A full HTTPS link. It must be on the installation's source host; links to other hosts are refused. A trailing `.git` is allowed. |
| `{ "ownerId": "…", "repositoryId": "…" }` | A repository by its stable provider IDs. Older Tasks use this form; it still works. |

The repository must be one the installation has approved. Steward never looks up a repository
it hasn't approved. If a repository named by `owner/name` or link isn't approved, the run is
refused with "The requested workspace repository is unavailable. Ask a Steward administrator to
verify that it is allowed and accessible." The ID form is refused with
`workspace_git_repository_not_admitted`. Either way, check the name and ask an administrator to
approve the repository.

`self` needs a run that was started from a repository, for example by GitHub Actions. If there
is none, the run is refused with `workspace_git_self_unavailable`.

## Choose the commit (`ref`)

Steward turns every `ref` into one exact commit when it accepts the run, before anything starts.
Run details show that commit. If you retry with the same idempotency key, the retry uses the
commit already recorded, even if the branch has moved since.

| `ref` value | Meaning |
|---|---|
| omitted or `"trigger"` | See [What `trigger` means](#what-trigger-means). |
| `"main"`, `"release/2026-11"`, `"v1.4.0"` | A branch or tag name, resolved to its current commit. |
| `"git:ref:v1.4.0"` | The same, written explicitly. Use this form when a tag or branch name looks like a commit ID (`"git:ref:deadbeef"`) or is literally `trigger`. |
| `"git:sha1:"` + 40 hex digits | One exact commit. |

These values are refused:

- `"HEAD"`;
- a bare commit ID such as `"3f2a9c1"` (write `"git:sha1:<40 hex digits>"` instead);
- names that start with `-`, contain `..`, `@{`, `\`, spaces or control characters, or end in `/`.

<!-- schema: invalid workspaceEntry -->
```json
{ "git": { "repository": "example-org/service-api", "ref": "HEAD" } }
```

An exact commit looks like this:

<!-- schema: workspaceEntry -->
```json
{ "git": { "repository": "example-org/service-api", "ref": "git:sha1:0123456789abcdef0123456789abcdef01234567" } }
```

If a branch or tag doesn't exist, the run is refused with a message such as "The branch or tag
release/2026-12 was not found in example-org/service-api." Avoid giving a branch and a tag the
same name; which one wins is up to the source host.

## What `trigger` means

`trigger` is the default `ref`. What it resolves to depends on the repository:

| Repository | Run started from a commit (for example GitHub Actions) | Run with no starting commit (for example the browser) |
|---|---|---|
| `self` | That exact commit. | The repository's default-branch HEAD. |
| Any other repository | That repository's **default-branch HEAD**. | The repository's default-branch HEAD. |

The second row also applies when you name the starting repository by `owner/name` instead of
`self`. Today that entry gets the default-branch HEAD, not the commit that started the run.
Issue [#659](https://github.com/apelogic-ai/steward/issues/659) tracks changing this, so that a
named starting repository gets the starting commit and `trigger` on any other repository is
refused.

To avoid surprises:

- use `"repository": "self"` for the repository that started the run;
- give every other repository an explicit branch, tag or `git:sha1:` commit.

## Changing the ref for one run

There is no per-run override. A Task request carries no workspace field, and a run can't change
a `ref`, because the refs are part of the Task package and its digest. To use a different commit:

- use `self` with `trigger`, so each run checks out the commit that started it;
- point the entry at a branch or tag and move that branch or tag; each new run resolves it again;
- edit the Task to change the `ref`, which makes a new Task version;
- for a one-off run in the browser, choose the repository and its branch or tag on **Run now**
  before you start the run.

## Git history

| `history` | What you get |
|---|---|
| omitted or `"none"` | Files only, with no `.git` directory. `git log` and `git blame` don't work. |
| `{ "depth": 50 }` | A `.git` directory with the last 50 commits, so `git log` and `git blame` work offline. |
| `"full"` | The complete history, when the installation and your Envelope allow it. |

The installation sets a maximum depth (1,000 commits unless an administrator changed it), and
your Envelope can lower it. There is no Git remote in the sandbox either way, so the agent can't
fetch or push.

## Check out only some paths

`paths` lists the files or directories you want. Everything else is left out, which keeps large
repositories small. End a directory with `/`. Paths are relative to the repository root and
can't contain `..` or start with `/`.

<!-- schema: workspaceEntry -->
```json
{ "git": { "repository": "example-org/monorepo", "ref": "main", "paths": ["services/billing/", "README.md"] } }
```

Submodules outside the selected paths are skipped.

## Submodules

Submodules are off unless you ask for them:

<!-- schema: workspaceEntry -->
```json
{
  "git": {
    "repository": "example-org/service-api",
    "ref": "main",
    "submodules": { "mode": "admitted", "required": ["vendor/protos"], "recursive": false }
  }
}
```

With `"mode": "admitted"`:

- **Only approved repositories.** A submodule is checked out only if its repository is one the
  installation has approved. Other submodules are skipped.
- **Always the pinned commit.** Each submodule is checked out at the commit recorded in the
  parent repository (its gitlink), never at a branch.
- **Files only.** A submodule never has a `.git` directory, whatever `history` says.
- **One level by default.** `"recursive": true` also checks out submodules of submodules, up to
  the depth the installation and your Envelope allow (4 levels unless an administrator changed
  it).
- **Required submodules.** If a path in `required` can't be checked out, the run is refused with
  `workspace_git_required_submodule_unavailable`. Other skipped submodules don't stop the run.

Run details list every submodule as checked out or skipped. A skipped one shows a reason:

| Reason | Meaning |
|---|---|
| `path_not_selected` | It is outside the entry's `paths`. |
| `gitlink_missing` | `.gitmodules` names it, but the parent commit has no pinned commit for it. |
| `repository_unavailable` | Steward couldn't find its repository on the source host. |
| `repository_not_admitted` | Its repository isn't approved for this installation. |

The installation also caps the number of submodules (32 per entry and 64 per run unless an
administrator changed it).

## Read-only or writable

| `access` | Meaning |
|---|---|
| omitted or `"read-only"` | The files can't be changed. |
| `"copy"` | A writable copy with no remote and no credentials. Changes leave the sandbox only through your Task's [outputs](outputs.md). |

## Scratch space and limits

A scratch entry is an empty writable directory of the size you ask for:

<!-- schema: workspaceEntry -->
```json
{ "scratch": { "name": "build", "size": "4Gi", "limits": { "files": 20000 } } }
```

Sizes use `Ki`, `Mi`, `Gi` or `Ti`. Any entry can also set `limits.size` and `limits.files`.
Steward applies the smallest of three limits: your entry's, your Envelope's and the
installation's. A run that needs more is refused with `workspace_limit_exceeded` before
anything starts. Your Envelope must also allow workspaces at all; see
[Concepts](concepts.md#workspace).

See the [reference](reference.md#workspace-entries) for every field and default.
