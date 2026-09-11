# Steward direct Git package Task contract

Contract identifier: **`steward.task/v2`**

This directory freezes the provider-neutral wire and artifact contract for invoking a
reviewed Task package directly from an exact Git object. It is additive: nothing under
`docs/contracts/m1/v1` changes, and a v2 parser never falls back to a v1 catalog
resolver.

The authoritative JSON Schema is
[`schemas/direct-package.schema.json`](schemas/direct-package.schema.json). Rust
consumers share the validated wire types and constants in
`steward_types::direct_package`.

## Submission and invocation

The reusable workflow creates a direct Task with only the contract selector and the
repository-relative invocation-manifest path:

```json
{
  "contractVersion": "steward.task/v2",
  "invocationPath": ".steward/tasks/release-summary.json"
}
```

The caller never uploads or supplies the invocation bytes. Steward fetches the path
from the Identity-ratified invoking repository at its exact triggered commit. Paths
are bounded canonical slash-separated paths: absolute paths, empty components, `.`,
`..`, backslashes, control characters, and symlink escapes are invalid.

The fetched invocation manifest has this exact shape:

```json
{
  "contractVersion": "steward.task/v2",
  "package": {
    "repository": "https://github.com/example-org/agentic-ops.git",
    "commit": "git:sha1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "path": "catalog/release-summary/v1/task-definition.json"
  },
  "envelope": "steward:sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "diagnostics": {
    "executionLog": "full"
  }
}
```

`repository` is a canonical credential-free HTTPS clone URL. `commit` is an exact
lowercase SHA-1 identity. `git:trigger` is the only symbolic value and is valid only
when the package repository equals the invoking repository; Steward immediately
resolves it to the verified `triggeredSha`. Branches, tags, abbreviated SHAs, and
caller-provided credentials fail closed.

`envelope` selects one active caller-authorized Envelope by approved content digest.
It is not an Envelope UUID. Internal Envelope UID and revision appear only in evidence.

Missing diagnostics, `{}`, or `executionLog: off` means no caller-visible execution
transcript. A v2 create/status representation always returns the server-snapshotted
effective diagnostics object. A runner requires that authenticated `full` signal as
well as the reserved files before replay; file presence alone grants nothing.

## TaskDefinition and instruction skills

The direct TaskDefinition schema is `steward.task-definition/v2`. It contains:

- a lowercase logical name and positive package version;
- an exact `runtime.agentRef` that selects advertised deployment metadata, never an
  image, executable, provider profile, or native policy;
- one prompt Markdown path relative to the TaskDefinition directory;
- zero or more instruction-skill descriptor paths, also relative to that directory;
- one or more workspace-relative declared outputs beneath `out`; and
- optional complete `requires`.

Omitted `skills` and `skills: []` both mean no skills. Duplicate descriptor paths are
invalid. An instruction-skill descriptor uses `steward.instruction-skill/v1`.
`kind` is optional and defaults to `instruction_only`; that is the only accepted kind.
Its instruction and asset paths resolve relative to the descriptor directory. A
future executable kind requires a new skill schema and explicit runtime and Envelope
authority; this v1 schema can never acquire executable meaning.

Omitted `requires` means the selected Envelope's entire approved execution and
authority values become effective. A present `requires` object is a complete narrower
candidate: it contains both `execution` and `authority`, and authority contains all of
`llms`, `tools`, `budget`, `ttl`, and `runner`. Empty arrays express an explicit empty
set. Partial objects are invalid. Steward snapshots the resulting complete effective
requirements in evidence before runtime work.

## Signed source provenance

Identity emits one structured `steward.source-provenance/v1` object in the signed
exchange-JWT claim named `source_provenance`. Steward's direct Identity resolver
consumes that claim after verifying the exchange JWT. Kubernetes TokenReview remains
a separate service-account authentication path and does not carry this provenance or
Identity user groups.

The signed object contains stable repository, owner, actor, and run IDs; human-readable
repository and actor metadata; exact triggered, caller-workflow, and reusable-workflow
SHAs; run attempt; event; Git ref; and the two workflow refs. IDs and exact SHAs are
authoritative. Display names are evidence metadata and never substitute for stable IDs.
Git and workflow refs allow 2,048 bytes to match the verified GitHub claim boundary;
other display metadata is limited to 512 bytes. The raw OIDC assertion, exchange JWT,
tokens, credentials, and provider authorization material are forbidden.

## Package closure and digest

The package root is the directory containing the TaskDefinition. References are
resolved as follows:

1. resolve `prompt` and every `skills` entry relative to the TaskDefinition directory;
2. parse each skill descriptor strictly, then resolve its `instructions` and `assets`
   relative to that descriptor's directory;
3. reject absolute paths, traversal, symlinks, duplicate logical paths, objects outside
   the package root, cross-repository references, cycles, and inconsistent exact-object
   reads; and
4. emit exactly one `task_definition` entry matching `entryPoint`, then all entries in
   ascending ASCII path order.

There are at most 128 files, each at most 8 MiB, and at most 16 MiB in total. JSON
descriptors must be UTF-8 without a BOM, must reject duplicate object keys and unknown
fields, and are represented by their RFC 8785 canonical bytes. Markdown and other
assets retain exact Git blob bytes. Each entry digest is
`steward:sha256:<sha256-bytes>` over that representation.

The closure document is `steward.package-closure/v1` and contains only its entry point
and sorted `(kind, path, digest, sizeBytes)` entries. The closure digest is SHA-256 over
the RFC 8785 encoding of that document. It deliberately excludes repository, commit,
Envelope, runtime input, Task identity, and the closure digest itself. The published
vector in [`vectors/package-closure-digest.json`](vectors/package-closure-digest.json)
is normative.

## Immutable Task binding evidence

`steward.task/source-authority-evidence/v1` is the server-authored immutable binding
fragment for a direct Task. It records:

- the Task UID and verified signed source provenance;
- invoking-manifest and package repository URLs, stable repository/owner IDs, exact
  commits, paths, and content digests;
- the complete canonical closure and closure digest;
- the resolved internal Envelope UID, revision, and approved digest;
- complete effective requirements; and
- the snapshotted effective diagnostic mode.

The caller cannot supply this shape. Retries must match it exactly and cannot switch
source bytes, authority, provenance, or diagnostics.

## Successful execution transcript

When and only when snapshotted diagnostics is `full`, a successful Task output archive
may contain two server-owned entries:

```text
.steward/diagnostics/stdout.log
.steward/diagnostics/stderr.log
```

Each original process stream is retained verbatim up to 4 MiB; the combined bound is
8 MiB. Exceeding either bound prevents a successful transcript result and returns the
bounded `execution_transcript_too_large` failure category rather than truncating
silently. The `.steward/diagnostics` namespace is reserved: agent output cannot create
or replace these entries. The runner replays them only after authentication, path and
size validation, and a sensitive-output warning.

The streams may reproduce prompts, model output, repository data, and tool results.
Injected credentials, bearer tokens, private keys, provider-control material, and
hidden model reasoning must never enter them. Durable logs for failed, cancelled, or
rejected Tasks remain outside this contract.

## Compatibility and validation

The Rust contract tests parse every positive semantic shape, exercise fail-closed
version, path, privilege-injection, duplicate, diagnostic, and cross-source cases,
verify canonical bytes, and compare the v1 compatibility fixture to the authoritative
frozen v1 fixture:

```bash
cargo test -p steward-types --test direct_package_contract
```

The normal repository gate remains `cargo xtask ci`.
