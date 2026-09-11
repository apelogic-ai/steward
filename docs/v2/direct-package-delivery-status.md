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
| DP-C01 | `apelogic-ai/steward` | merged | PR `#83` merged as `ba4c062`; reviewed implementation head `103919e` was full-gate green. |
| DP-I01 | `github-oidc-exchange` | merged | PR `#31` merged as `f2e33ae`; provenance, amd64 quality/Trivy, native ARM, and Kubernetes replay were green. |
| DP-G01 | `apelogic-ai/steward` | merged | PR `#84` merged as `8c2ca03`; reviewed head `b93d611` was focused-, full-gate-, conformance-, and pre-push-green. |
| DP-S01 | `apelogic-ai/steward` | merged | PR `#85` merged as `3d6a76b`; reviewed head `99c5e0b` was full-gate-, Postgres-, and OpenShell-E2E-green. Broader adversarial coverage is recorded as post-demo debt. |
| DP-R01 | `apelogic-ai/steward-run` | merged | Pin-correction PR `#33` merged as `139221e`; repository and complete Action round-trip were green. |
| DP-R02 | `apelogic-ai/steward-run` | merged | PR `apelogic-ai/steward-run#32` merged as `a7bc15c`; live scan reported zero critical and zero accepted findings. |
| DP-A01 | `apelogic-ai/agentic-ops` | merged | PR `apelogic-ai/agentic-ops#4` merged as `cca19bc`; package validation is green. |
| DP-O01 | `apelogic-ai/gitops` | review | Final activation PR `apelogic-ai/gitops#177` at exact head `5f4bac1` is locally and CI green, clean, and mergeable. No cluster mutation. |
| DP-P01 | `apelogic-ai/steward` | post-P0 | Optional catalog publication and release provenance. |
| DP-T01 | `apelogic-ai/steward` | post-P0 | Multiple-package/workflow and stable-lane conformance. |
| DP-D01 | `apelogic-ai/steward` | post-P0 | Durable failed, cancelled, and rejected Task diagnostics. |

## Immutable handoffs

Record only reviewed commits, pushed branch heads, PRs, release identities, schema
digests, and exact source pins here. Do not record credentials, local run artifacts,
or mutable environment state.

| Producer | Consumer | Handoff | State |
|---|---|---|---|
| DP-C01 | DP-I01, DP-G01, DP-S01, DP-R01, DP-A01 | merged source `ba4c062`, reviewed implementation `103919e`; neutrality maintenance merged as `cc19487` | complete |
| DP-I01 | DP-S01, DP-O01 | merged source `f2e33ae`, implementation/integrated head `1f9fe99`, contract checkpoint `042e8aa`; native-ARM maintenance merged as `6d4c106` | ready for immutable deployment pin |
| DP-G01 | DP-S01, DP-O01 | merged source `8c2ca03`, reviewed implementation head `b93d611` | complete |
| DP-R02 | DP-R01 | merged commit `a7bc15c`, implementation `e73f931`, green live scan | incorporated in `79b9f42` |
| DP-R01 | DP-O01 | usable reusable-workflow pin `139221e`, nested action pin `fd090be`, implementation `79b9f42`, contract checkpoint `042e8aa` | ready for caller pin |
| DP-A01 | DP-O01 | merged source commit `cca19bc`, closure `steward:sha256:79a68a6e3f7a21d37c4da0594555d409999562b3d1778641f743ec0e290118a8` | ready for caller pin |
| DP-S01 | DP-O01 | merged source `3d6a76b`, reviewed head `99c5e0b`; full gate, 5/5 real-Postgres regressions, and authenticated OpenShell transcript E2E green with cleanup verified | complete |
| DP-O01 fixture | DP-O01 activation | merged GitOps source `f8ec588`, reviewed implementation head `e48300b` | complete |
| DP-O01 | DP-P00 | PR `apelogic-ai/gitops#177`, exact head `5f4bac1`; full local repository validation and GitHub `manifests` check green | merge pending, then GitOps-owned activation |

## Operating constraints

- Every ticket uses its own repository-local worktree and branch.
- Downstream agents may prepare tests before DP-C01 completes but may not invent or
  freeze conflicting field names.
- Heavy integration lanes are serialized.
- The GitOps-owned local-main environment is never used for implementation testing.
- No agent merges or closes a PR or modifies repository protection or rulesets.
