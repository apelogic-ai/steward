# Direct Git package Task invocation

Status: accepted implementation architecture

This document defines the post-M1-v1 path for invoking a reviewed agent Task
package directly from Git. It keeps Git as the source of record, preserves the
administrator-approved Steward Envelope as the execution authority, and uses the
common Task application service delivered by PR 78.

The first implementation is intentionally narrow enough for the local demo while
leaving non-breaking extension points for additional Git providers, skill kinds,
catalog publication, and richer diagnostics.

## Outcome

A developer or platform engineer authors a versioned Task package in an admitted
operations repository. A workflow in an admitted caller repository references the
exact package source and an approved Envelope digest through a checked-in invocation
manifest. Steward resolves the source itself, admits the effective request, executes
the Task with the caller's Steward Principal, and returns declared outputs and an
optional execution transcript to the GitHub Actions run.

Catalog publication is not required for this path. Publication remains an optional
discovery and curation layer over immutable Git packages.

## Trust and authority model

The path separates four concerns:

1. **Source approval.** Repository governance reviews and merges package and
   invocation-manifest changes. Steward does not query pull-request review state or
   branch reachability.
2. **Source authorization.** The external GitHub repository identity must be admitted
   for the caller. Cross-repository use requires an explicit caller-to-source binding.
3. **Execution authority.** An administrator creates or selects an Envelope in
   Steward, approves it in the Steward UI, and places its content digest in the
   invocation manifest. Steward's active caller-to-Envelope binding is authoritative.
4. **Task ownership.** The existing server-resolved Steward `Principal` owns the Task,
   runtime, provider credentials, and budget. Git identities do not replace it.

Internal database identifiers are never caller-facing authority. Steward may record
the concrete Envelope instance and revision in evidence, but callers select an
Envelope only by its approved SHA-256 content digest.

Old Git objects remaining fetchable does not keep an authorization active. Steward
accepts a source only while the external repository identity and the corresponding
caller-to-source binding remain active.

## GitHub Actions contract

The invoking repository checks in a manifest and passes only its workspace-relative
path to the reusable workflow:

```yaml
jobs:
  governed:
    uses: apelogic-ai/steward-run/.github/workflows/steward-task.yml@<pinned-commit>
    with:
      invocation-path: .steward/tasks/release-summary.json
```

An initial cross-repository manifest is:

```json
{
  "contractVersion": "steward.task/v2",
  "package": {
    "repository": "https://github.com/apelogic-ai/agentic-ops.git",
    "commit": "git:sha1:<40-hex>",
    "path": "catalog/release-summary/v1/task-definition.json"
  },
  "envelope": "steward:sha256:<64-hex>",
  "diagnostics": {
    "executionLog": "full"
  }
}
```

The fields have these meanings:

- `repository` is a canonical clone URL. It remains separate from `commit` so both
  ordinary Git and provider CLIs can consume it without a custom combined URI.
- `commit` is an exact Git object identity. `git:trigger` is also allowed, but only
  when the package repository is the invoking repository; Steward resolves it to the
  exact triggered SHA before admission.
- `path` is a repository-relative package entry point with no traversal or symlink
  escape.
- `envelope` is the digest of an active administrator-approved Steward Envelope.
- `diagnostics.executionLog` is optional. Missing means `off`; `full` requests the
  successful-run transcript described below.

The GHA workflow supplies per-run inputs separately through the existing Task input
archive flow. Runtime input is not portable package content and is not part of the
package closure digest.

## Verified source capture

`steward-run` exchanges GitHub's OIDC token through the existing Identity path. The
exchange and TokenReview result must preserve the verified source provenance required
for this contract, including stable repository and owner IDs, exact triggered SHA,
run and run-attempt IDs, event and ref, actor, and caller and reusable-workflow refs
and SHAs.

`steward-run` submits `invocation-path` and the ratified trigger metadata. It does not
upload the invocation manifest or attest to package bytes.

Steward resolves source through a provider-neutral Git source port. The first adapter
uses a read-only GitHub App installed only on admitted repositories. Steward mints
short-lived installation tokens with metadata-read and contents-read permissions. It
never accepts the workflow `GITHUB_TOKEN`, a PAT, or caller-supplied Git credentials.

Steward fetches:

1. the invocation manifest from the invoking repository at the verified triggered
   commit;
2. the package entry point from its declared repository at the resolved exact commit;
3. every package-local dependency required by that entry point.

It rejects mutable refs, missing objects, repository-identity mismatches, cross-source
dependencies, path traversal, symlink escapes, ambiguous encodings, cycles, duplicate
logical paths, and content that changes while resolving the same exact identity.

The provider-neutral interface leaves room for GitLab, Gitea, and generic Git
adapters. Those adapters are deferred and must provide equivalent stable repository
identity, exact-object retrieval, authorization, and audit properties.

## Package contract

Direct packages use new `steward.task/v2` schemas. The frozen `steward.m1/v1`
catalog contract is unchanged and there is no v2 fallback to the legacy or catalog
resolver.

The package entry point contains the prompt, runtime selection, declared outputs,
optional skill descriptors, and optional narrower authority requirements. The exact
wire schemas are owned by the contract ticket, but these semantics are fixed:

- `skills` omitted or `[]` means no skills;
- zero or multiple package-local skills are valid;
- an omitted skill kind means `instruction_only`;
- the initial implementation accepts instruction-only skills;
- a future executable-skill kind requires a new schema version plus explicit runtime
  capability and Envelope permission;
- existing instruction-only packages must never become executable implicitly;
- `requires` omitted means use the selected Envelope's complete approved values;
- `requires` present is a complete, explicit request that must be no broader than the
  selected Envelope; and
- Steward records the fully expanded effective requirements in Task evidence.

The initial missing-`requires` behavior favors authoring simplicity over least
privilege. Operators should therefore select a deliberately bounded Envelope.

Steward computes a deterministic digest over the resolved package closure and records
the repository identity, exact commit, entry path, dependency identities and digest,
Envelope digest, effective requirements, source provenance, and Task/runtime
identities. The human-facing reference remains repository, commit, and path.

## Admission and execution

The direct-package resolver produces the same admitted Task command consumed by the
common Task application service. It does not create another desired-state write door.

The server performs, in order:

1. authenticate the existing Steward `Principal`;
2. verify Identity-ratified GitHub trigger provenance;
3. fetch and validate the invocation manifest at the triggered commit;
4. authorize the invoking and package repository identities;
5. resolve and snapshot the exact package closure;
6. locate an active Envelope for that Principal with the requested digest;
7. expand package requirements and run normal Steward admission;
8. reserve the Task idempotently with immutable source and authority evidence;
9. bind or provision the exact runtime through the common PR-78 orchestration path;
10. accept the existing per-run input archive;
11. execute through OpenShell, LiteLLM, and MCP-GW; and
12. return declared outputs before ordinary finalization and cleanup.

Package source approval never grants provider access. GitHub MCP access still requires
the active Envelope, a valid user connection, runtime-bound token-grant authority, and
MCP-GW enforcement.

## P0 execution transcript

For the initial demo, `diagnostics.executionLog: full` requests a successful-run
transcript. The coding-agent process's original stdout and stderr are captured as
separate files in the server-owned Task output area. After successful execution,
`steward-run` downloads them with the authenticated output archive and replays them
verbatim into clearly labelled GitHub Actions log groups.

Before replay, `steward-run` emits the documented sensitive-output warning. The
transcript may reproduce prompts, repository data, model output, and MCP results. It
must never include injected credentials, bearer tokens, private keys, or provider
authorization material. Provider-control executions remain structurally excluded.

Missing diagnostics or `executionLog: off` captures no caller-visible transcript.
The diagnostic selection is snapshotted with the Task.

This P0 contract deliberately covers successful execution only. Durable logs for
failed, cancelled, or pre-execution-rejected Tasks require a separate bounded
observability design: retention, authorization, size limits, redaction boundaries,
failure-safe persistence, and a Task-log API. Lifecycle summaries must not be
presented as a substitute for those logs.

## P0 demonstration

The demo uses:

- caller repository: `apelogic-ai/gitops`;
- package repository: `apelogic-ai/agentic-ops` at an exact commit;
- package: one release-summary prompt, no skills, and omitted `requires`;
- input: a validated workflow-dispatch URL for a run in `apelogic-ai/gitops`;
- authority: an administrator-approved Envelope selected by digest;
- provider: real read-only GitHub MCP calls through MCP-GW; and
- output: `out/release-summary-<run-id>.md` plus the enabled successful-run
  stdout/stderr transcript.

The caller workflow validates that the input is exactly
`https://github.com/apelogic-ai/gitops/actions/runs/<numeric-run-id>` before creating
`in/request.json`. This is an authoring-time usability check, not the provider security
boundary.

The prompt inspects real run metadata, jobs, job logs, components, produced artifacts,
security/SBOM evidence when present, test results, and failures. It reports only
observed facts and uses no customer names or data. The end-to-end acceptance run must
prove that at least one real GitHub MCP call occurred; mocks remain limited to unit
tests.

## Delivery plan

| Ticket | Deliverable | May begin |
|---|---|---|
| DP-C01 | Freeze v2 manifest, package, skill, evidence, and diagnostic schemas | First |
| DP-I01 | Preserve verified GitHub source provenance through Identity | After DP-C01 |
| DP-G01 | Add the provider-neutral Git source port and read-only GitHub adapter | After DP-C01 |
| DP-R01 | Add `invocation-path`, trigger transport, inputs, and transcript replay to `steward-run` | After DP-C01 |
| DP-A01 | Author the neutral release-summary package in `agentic-ops` | After DP-C01 |
| DP-S01 | Resolve, admit, persist, and execute direct packages through the common Task service | After DP-G01; tests may start earlier |
| DP-O01 | Activate the UI-approved Envelope and GHA demo integration | Preparation may start early; live pins wait for releases |
| DP-P00 | Track the complete local demonstration and its evidence | Throughout |

DP-I01, DP-G01, DP-R01, and DP-A01 are safe to implement in parallel after the
contract freezes. DP-S01 consumes DP-G01. DP-O01 must not use the GitOps-owned
local-main cluster for development or testing; only GitOps performs the final
state-preserving activation and demo verification after reviewed releases are ready.
Only one heavy local integration lane runs at a time.

## Deferred, non-blocking extensions

- server-side GitLab, Gitea, and generic Git adapters;
- Git-managed caller-to-source authorization-policy files;
- catalog publication, tags, and release provenance;
- executable skill kinds under a new schema version;
- authenticated admission preview and authoring-time explain commands;
- [durable failed/cancelled Task logs and a Task-log API](task-execution-observability-ticket.md);
- multiple-package and multiple-workflow isolation testing; and
- stable-lane coverage beyond the P0 demonstration.

## Architectural guarantees

- Git is the package system of record; Steward stores immutable evidence, not a
  competing caller-facing package identity.
- Envelope authority remains administrator approved and server bound.
- Exact source resolution and admission occur before Task reservation or runtime work.
- Every desired-state write uses Steward admission and the common Task application
  service.
- Cross-repository access is explicit and revocable.
- Package bytes, input bytes, diagnostics choices, and authority are independently
  identified and cannot silently substitute for one another.
- Direct invocation adds no legacy fallback and does not change frozen v1 behavior.
