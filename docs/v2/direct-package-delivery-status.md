# Direct-package delivery status

Status: active coordination record

Updated: 2026-09-10

This file tracks implementation state and exact cross-repository handoffs for the
[accepted direct-package architecture](direct-package-task-invocation.md). Ticket
contracts remain authoritative; this record does not relax their exit criteria.

## Active dependency graph

| Ticket | Repository | State | Current dependency or handoff |
|---|---|---|---|
| DP-P00 | `apelogic-ai/steward` | in progress | Coordinates the complete demo evidence. |
| DP-C01 | `apelogic-ai/steward` | maintenance review | Corrected contract checkpoint `d4d8dc4` is focused-green; isolated neutrality maintenance PR `apelogic-ai/steward#82` at `fa921f6` awaits review before the full gate. |
| DP-I01 | `github-oidc-exchange` | maintenance review | PR `apelogic-ai/github-oidc-exchange#31` at `4f7a685` is locally green; isolated PR `#32` at `13a89de` proves the same ARM smoke natively and is fully green. |
| DP-G01 | `apelogic-ai/steward` | queued | Starts after DP-C01 publishes the contract commit and an agent slot is free. |
| DP-S01 | `apelogic-ai/steward` | red checkpoint | Five pre-reservation escape tests are retained locally at `bf53e91`; implementation depends on DP-G01 and DP-I01. |
| DP-R01 | `apelogic-ai/steward-run` | merged | PR `apelogic-ai/steward-run#31` merged as `fd090be`; image CI, vulnerability policy, and full round-trip were green. |
| DP-R02 | `apelogic-ai/steward-run` | merged | PR `apelogic-ai/steward-run#32` merged as `a7bc15c`; live scan reported zero critical and zero accepted findings. |
| DP-A01 | `apelogic-ai/agentic-ops` | merged | PR `apelogic-ai/agentic-ops#4` merged as `cca19bc`; package validation is green. |
| DP-O01 | `apelogic-ai/gitops` | waiting | Preparation follows reviewed releases; GitOps alone operates local-main. |
| DP-P01 | `apelogic-ai/steward` | post-P0 | Optional catalog publication and release provenance. |
| DP-T01 | `apelogic-ai/steward` | post-P0 | Multiple-package/workflow and stable-lane conformance. |
| DP-D01 | `apelogic-ai/steward` | post-P0 | Durable failed, cancelled, and rejected Task diagnostics. |

## Immutable handoffs

Record only reviewed commits, pushed branch heads, PRs, release identities, schema
digests, and exact source pins here. Do not record credentials, local run artifacts,
or mutable environment state.

| Producer | Consumer | Handoff | State |
|---|---|---|---|
| DP-C01 | DP-I01, DP-G01, DP-S01, DP-R01, DP-A01 | corrected checkpoint `d4d8dc4`; neutrality maintenance PR `apelogic-ai/steward#82` at `fa921f6` | full gate waits for maintenance merge |
| DP-I01 | DP-S01, DP-O01 | PR `apelogic-ai/github-oidc-exchange#31`, commit `4f7a685`, contract checkpoint `042e8aa`; fully green native-ARM CI PR `#32` at `13a89de` | maintenance review pending |
| DP-G01 | DP-S01, DP-O01 | reviewed Git source adapter release/configuration contract | pending |
| DP-R02 | DP-R01 | merged commit `a7bc15c`, implementation `e73f931`, green live scan | incorporated in `79b9f42` |
| DP-R01 | DP-O01 | merged source commit `fd090be`, implementation `79b9f42`, contract checkpoint `042e8aa` | ready for immutable caller pin |
| DP-A01 | DP-O01 | merged source commit `cca19bc`, closure `steward:sha256:79a68a6e3f7a21d37c4da0594555d409999562b3d1778641f743ec0e290118a8` | ready for caller pin after DP-C01 is published |
| DP-S01 | DP-O01 | reviewed Steward release and E2E verifier | pending |
| DP-O01 | DP-P00 | local-main preflight and final demo evidence | pending |

## Operating constraints

- Every ticket uses its own repository-local worktree and branch.
- Downstream agents may prepare tests before DP-C01 completes but may not invent or
  freeze conflicting field names.
- Heavy integration lanes are serialized.
- The GitOps-owned local-main environment is never used for implementation testing.
- No agent merges or closes a PR or modifies repository protection or rulesets.
