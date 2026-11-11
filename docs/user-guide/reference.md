# Reference

The tables summarize the Task contract for people writing Tasks. The complete, authoritative
rules are in the [Task contract](../contracts/task/v2/README.md) and its
[JSON Schema](../contracts/task/v2/schemas/direct-package.schema.json).

## Workspace entries

A `workspace` array holds up to 32 entries. Each is either `{ "git": { … } }` or
`{ "scratch": { … } }`.

### Git entry

| Field | Default | Values |
|---|---|---|
| `name` | Repository name, lowercased and slugged; see [defaults](repositories-and-workspaces.md#where-the-checkout-goes) | 1–40 of `a-z`, `0-9`, `-` |
| `repository` | `"self"` | `"self"`, `"owner/name"`, `"https://<source host>/owner/name"`, or `{ "ownerId", "repositoryId" }` |
| `ref` | `"trigger"` | See [ref forms](#ref-forms) |
| `history` | `"none"` | `"none"`, `{ "depth": N }` with N ≥ 1, or `"full"` |
| `paths` | `[]`, meaning everything | Relative files or directories (directories end in `/`); no `..`, no leading `/` |
| `submodules.mode` | `"none"` | `"none"` or `"admitted"` |
| `submodules.required` | `[]` | Submodule paths that must be checked out |
| `submodules.recursive` | `false` | `true` also checks out nested submodules |
| `access` | `"read-only"` | `"read-only"` or `"copy"` (writable, no remote) |
| `limits.size` | none | A size such as `"500Mi"` or `"2Gi"` |
| `limits.files` | none | A whole number ≥ 1 |

### Scratch entry

| Field | Default | Values |
|---|---|---|
| `name` | `"scratch"` | 1–40 of `a-z`, `0-9`, `-` |
| `size` | required | A size such as `"1Gi"` |
| `limits.size`, `limits.files` | none | As for Git entries |

### Ref forms

| Form | Example | Resolves to |
|---|---|---|
| `trigger` | `"trigger"` | For `self`: the commit that started the run. Otherwise, or with no starting commit: the default-branch HEAD. See [#659](https://github.com/apelogic-ai/steward/issues/659). |
| Branch or tag | `"main"`, `"v1.4.0"` | Its commit when the run is accepted |
| Explicit name | `"git:ref:v1.4.0"` | Same as a branch or tag; required for names that look like commit IDs |
| Exact commit | `"git:sha1:0123…"` (40 hex digits) | That commit |
| Refused | `"HEAD"`, bare commit IDs, names with `..`, `@{`, `\`, spaces, a leading `-` or a trailing `/` | — |

## Requirements

`requires.authority` must contain every field below except `runtimeMinutesLimit`. Each value must
be within your Envelope.

| Field | Format | Example | Meaning |
|---|---|---|---|
| `llms` | List of `{ "provider", "model" }` | `[{ "provider": "litellm", "model": "review-model" }]` | Models the run may call. `[]` means none. |
| `tools` | List of `{ "provider", "resource", "action" }` | `[{ "provider": "cli", "resource": "kubectl", "action": "read" }]` | Tools the run may use. `[]` means none. |
| `budget.monthlyLimit` | Decimal string | `"50.00"` | Monthly spend for this Task's runs. |
| `budget.singleRunLimit` | Decimal string or `null` | `"2.00"` | Spend cap per run. `null` means no cap, which an Envelope with a per-run limit refuses. |
| `budget.currency` | Three capital letters | `"USD"` | Must match the Envelope. |
| `ttl` | Number with `s`, `m` or `h` | `"45m"` | Maximum run duration. |
| `runtimeMinutesLimit` | Decimal string, optional | `"120"` | Runtime minutes per month the Task asks for. |
| `runner.platforms` | List of `linux`, `mac`, `windows` | `["linux"]` | Allowed runner platforms. |
| `runner.memory`, `runner.compute`, `runner.storage` | Size string or `null` | `"2Gi"`, `"1000m"` | Requested runner size. `null` means no specific request. |

A complete example:

<!-- schema: directRequirements -->
```json
{
  "authority": {
    "llms": [{ "provider": "litellm", "model": "review-model" }],
    "tools": [{ "provider": "cli", "resource": "kubectl", "action": "read" }],
    "budget": { "monthlyLimit": "50.00", "singleRunLimit": null, "currency": "USD" },
    "ttl": "45m",
    "runtimeMinutesLimit": "120",
    "runner": { "platforms": ["linux"], "memory": "2Gi", "compute": null, "storage": null }
  }
}
```

Leaving out a required field is refused:

<!-- schema: invalid directRequirements -->
```json
{ "authority": { "llms": [], "tools": [], "ttl": "1h" } }
```

## Limits

Installation limits are the defaults; an administrator can change them, and your Envelope can
lower them further.

| Limit | Value |
|---|---|
| Workspace entries per Task | 32 |
| Workspace total size | 2 GiB (installation default) |
| Files per workspace | 100,000 (installation default) |
| Git history depth | 1,000 commits (installation default) |
| Submodule depth | 4 levels (installation default) |
| Submodules | 32 per entry, 64 per run (installation default) |
| Outputs per Task | 1 to 32 declared paths, all under `out` |
| Output archive per run | 64 MiB |
| Inline prompt (`promptText`) | 32 KiB |
| Task package | 128 files, 8 MiB per file, 16 MiB in total |
| Browser inline files | 64 KiB in total |
| Browser `inputs` object | 16 KiB |
| Browser runs in progress | 4 per person |
| Idempotency key | Required for API callers; 1 to 255 bytes. The GitHub action accepts 1–128 characters from `A-Z a-z 0-9 . _ : -`. |
