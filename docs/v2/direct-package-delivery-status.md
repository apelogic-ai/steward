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
| DP-C01 | `apelogic-ai/steward` | in progress | Contract/schema lane; blocks serialization freeze in downstream lanes. |
| DP-I01 | `github-oidc-exchange` | in progress | Verified provenance belongs in signed exchange-JWT claims consumed by Steward's direct Identity resolver; synchronize exact fields to DP-C01. |
| DP-G01 | `apelogic-ai/steward` | queued | Starts after DP-C01 publishes the contract commit and an agent slot is free. |
| DP-S01 | `apelogic-ai/steward` | blocked | Test preparation follows DP-C01; implementation depends on DP-G01. |
| DP-R01 | `apelogic-ai/steward-run` | in progress | Seam inventory and red tests may proceed; synchronize request names to DP-C01. |
| DP-A01 | `apelogic-ai/agentic-ops` | queued | Starts after DP-C01 publishes the contract commit and an agent slot is free. |
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
| DP-C01 | DP-I01, DP-G01, DP-S01, DP-R01, DP-A01 | exact contract commit and schema paths | pending |
| DP-I01 | DP-S01, DP-O01 | reviewed Identity release and provenance claim contract | pending |
| DP-G01 | DP-S01, DP-O01 | reviewed Git source adapter release/configuration contract | pending |
| DP-R01 | DP-O01 | reviewed reusable-workflow commit/release | pending |
| DP-A01 | DP-O01 | exact package commit, path, and closure validation | pending |
| DP-S01 | DP-O01 | reviewed Steward release and E2E verifier | pending |
| DP-O01 | DP-P00 | local-main preflight and final demo evidence | pending |

## Operating constraints

- Every ticket uses its own repository-local worktree and branch.
- Downstream agents may prepare tests before DP-C01 completes but may not invent or
  freeze conflicting field names.
- Heavy integration lanes are serialized.
- The GitOps-owned local-main environment is never used for implementation testing.
- No agent merges or closes a PR or modifies repository protection or rulesets.
