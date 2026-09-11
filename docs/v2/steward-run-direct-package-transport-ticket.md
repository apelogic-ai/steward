# DP-R01: `steward-run` direct-package transport and transcript replay

Priority: P0

Status: implementation merged by `apelogic-ai/steward-run#31` as `fd090be`; immutable
nested-action pin correction merged by `#33` as `139221e`

## Goal

Expose a minimal reusable-workflow interface for direct Git packages while preserving
the existing authenticated Task input, polling, output, finalization, and failure
metadata behavior.

## Scope

- add `invocation-path` for the direct-package flow, mutually exclusive with the
  legacy `workflow` input;
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

## Implementation evidence

- contract checkpoint: Steward DP-C01 commit `042e8aa`;
- implementation: `apelogic-ai/steward-run#31` at commit `79b9f42`;
- immutable merged source commit:
  `fd090be213b3f4d777bcfacc367e0cdbd574400b`;
- full `npm run check`: green, including 151 tests, typecheck, build, thin-shell
  validation, and checked-in bundle verification; and
- bundled E2E: green for path-only v2 submission, authenticated transcript replay,
  workflow-command suppression, output handling, and unconditional finalization.

The earlier CI vulnerability gate reported four affected GLib packages for
CVE-2026-58016 in the pinned Actions runner base image. DP-R02 removed that build-only
package chain without accepting the finding or weakening the policy and is now merged.
The current DP-R01 head contains that remediation; image CI, live vulnerability-policy
enforcement, and the complete round-trip are green.

## Post-merge handoff correction

The merged self-hosted reusable workflow exposes `invocation-path`, but it still
invokes nested action commit `0707623`, whose action contract is legacy-only. Therefore
`fd090be` is not itself a usable immutable caller pin despite the implementation and
round-trip being green on the feature branch. An isolated follow-up must pin that
nested action to the merged direct-transport implementation, rerun the complete
workflow/action contract and round-trip, and produce the new reusable-workflow commit
for DP-O01. No mutable ref is an acceptable substitute.

The focused red proof rejected the legacy-only nested action pin. At correction commit
`2cbfe5f`, all 152 repository tests, build, thin-shell, checked-in distribution, and
production dependency audit are green. GitHub CI and the complete seed, governed, and
verification Action round-trip are also green. DP-O01 uses merge commit `139221e` as
the reusable-workflow pin; its nested action remains pinned to `fd090be`.
