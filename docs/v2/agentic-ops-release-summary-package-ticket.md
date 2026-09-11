# DP-A01: Neutral release-summary package

Priority: P0

Status: merged by `apelogic-ai/agentic-ops#4` as
`cca19bc5671d46992037e928f55b1ae18f895730`

## Goal

Create the pre-reviewed, pre-approved demonstration package in
`apelogic-ai/agentic-ops` using the direct-package v2 contract.

## Package behavior

The package accepts one per-run `request.json` containing a GitHub Actions run URL for
the neutral `apelogic-ai/gitops` fixture. Its single prompt uses GitHub MCP to inspect
the entire run and writes a concise, copy-ready Markdown release summary to
`out/release-summary-<run-id>.md`.

The prompt should inspect, when present:

- run identity, workflow, ref, commit, status, conclusion, and duration;
- jobs, failing steps, and bounded failure details;
- resolved component revisions from repository metadata and logs;
- artifacts actually produced by the run;
- SBOM or security evidence actually produced by the run; and
- relevant test outcomes.

It must report only observed facts, use meaningful Markdown links, avoid bare URLs,
and omit absent sections unless their absence caused the run to fail.

## Contract choices

- one prompt;
- no skills;
- omitted `requires`, therefore the selected bounded Envelope supplies the effective
  authority;
- exact declared Markdown output;
- real read-only GitHub MCP tools, including the minimum actions and job-log calls
  required by the fixture; and
- no customer identities, product names, repositories, data, or copied proprietary
  wording.

## Validation

- schema and closure validation run in the authoring repository;
- prompt tests use sanitized fixtures and do not assert invented data;
- the exact package commit is reviewed before being pinned by the caller manifest;
- the final E2E run uses live GitHub MCP, not a mock; and
- the verifier demonstrates at least one real provider call and validates the output
  against the selected run.

## Exit criteria

- package source is merged at an immutable commit;
- closure validation and package tests pass;
- the invocation manifest can reference its repository, commit, and path directly;
- the generated report meets the neutral fixture contract; and
- no catalog publication, GitHub tag, or release is required.

## Parallel boundary

May proceed alongside DP-I01, DP-G01, and DP-R01 after DP-C01. It does not require a
running Steward environment until final integration.

## Implementation evidence

- contract checkpoint: Steward DP-C01 commit
  `042e8aa96a66d8927b50dc253509a5d437a5eeec`;
- implementation: `apelogic-ai/agentic-ops#4` at commit
  `5a2930ee6ab40e8e0a6b1c0e0bbbe46e8b1bb367`;
- immutable merged source commit:
  `cca19bc5671d46992037e928f55b1ae18f895730`;
- closure digest:
  `steward:sha256:79a68a6e3f7a21d37c4da0594555d409999562b3d1778641f743ec0e290118a8`;
  and
- repository validation: green for 95 tests, both package validators, syntax, and
  diff checks; and
- GitHub validation: green.
