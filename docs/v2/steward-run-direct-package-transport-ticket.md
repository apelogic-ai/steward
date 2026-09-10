# DP-R01: `steward-run` direct-package transport and transcript replay

Priority: P0

Status: blocked only on DP-C01 contract freeze

## Goal

Expose a minimal reusable-workflow interface for direct Git packages while preserving
the existing authenticated Task input, polling, output, finalization, and failure
metadata behavior.

## Scope

- add required `invocation-path` input for the v2 reusable workflow;
- send the path and GitHub trigger metadata, never manifest or package bytes;
- retain the current per-run input archive flow;
- poll the same Task resource and download declared outputs after success;
- recognize the reserved successful-run stdout/stderr entries when full diagnostics
  were selected;
- emit the documented sensitive-output warning;
- replay stdout and stderr verbatim into separate labelled GHA log groups; and
- finalize the Task on every success, failure, cancellation, and interrupt path.

Transcript replay must not attempt to parse hidden model reasoning or reinterpret
agent-specific JSON events. It presents the original process streams.

## Negative tests

- absolute, escaping, symlinked, or missing invocation paths are rejected locally;
- no workflow token, Git credential, package archive, or Envelope UUID is submitted;
- missing or malformed reserved transcript entries cannot overwrite workspace files;
- transcript output is never replayed without the snapshotted full-diagnostics choice;
- GitHub workflow-command injection is neutralized by using safe log grouping and data
  handling; and
- finalization still runs if output extraction or transcript replay fails.

## Exit criteria

- action inputs, outputs, documentation, and contract tests cover the v2 flow;
- successful unit and bundled E2E tests prove path transport and transcript replay;
- failure metadata remains bounded and compatible; and
- the reusable workflow can be pinned immutably by DP-O01.

## Parallel boundary

May proceed alongside DP-I01, DP-G01, and DP-A01 after DP-C01. It uses a mock server
for client tests; the real MCP proof belongs to the final P0 integration.
