# DP-I01: Preserve verified GitHub source provenance

Priority: P0

Status: native-ARM maintenance merged as `6d4c106`; provenance PR `#31` at `4f7a685`
is integrating current `main` and rerunning CI

## Goal

Extend the existing GitHub OIDC exchange JWT and Steward
`IdentityTaskIdentityResolver` so Steward can bind a direct-package request to the
actual repository, workflow, run, and commit verified by Identity.

## Scope

- validate the required GitHub OIDC claims under the existing issuer, audience, and
  pinned reusable-workflow policy;
- carry stable repository and owner IDs, repository name, exact SHA, run and
  run-attempt IDs, event, ref, actor, and caller/reusable-workflow refs and SHAs through
  the exchanged identity;
- expose the ratified values as signed structured exchange-JWT claims verified by the
  existing Identity resolver;
- retain the existing canonical user and group mapping; and
- define bounded failure categories suitable for the caller without echoing tokens or
  raw assertions.

This is additive provenance on the existing Identity-to-Principal path. It does not
create another identity system, encode source provenance into Kubernetes groups, send
the exchanged GitHub JWT through Kubernetes TokenReview, or let Git metadata own
Steward runtime authority. Kubernetes TokenReview remains the alternate
service-account authentication path.

## Negative tests

- a caller-submitted repository, SHA, run ID, or workflow ref cannot override a
  verified claim;
- a token from another repository or unpinned reusable workflow is rejected;
- repository names cannot substitute for mismatched stable repository IDs;
- absent required claims fail closed; and
- raw OIDC tokens and assertions never enter logs or Task evidence.

## Exit criteria

- positive exchange and direct Identity-resolver tests expose every DP-C01 provenance
  field;
- mutation and replay tests fail before Steward source resolution;
- existing user identity behavior remains compatible; and
- Steward can verify `invocation-path` against the ratified trigger repository and
  exact SHA.

## Parallel boundary

May proceed alongside DP-G01, DP-R01, and DP-A01 after DP-C01. It does not require the
GitOps-owned local-main cluster.

## Implementation evidence

- implementation: `apelogic-ai/github-oidc-exchange#31` at commit `4f7a685`;
- contract checkpoint: Steward DP-C01 commit `042e8aa`;
- exact provenance field compatibility: independently reviewed with no contract
  corrections required;
- repository dot-segment and noncanonical run-attempt mutations: rejected by focused
  regressions; and
- local Rust, Helm, release validation, amd64 image build, and container smoke gates:
  green; and
- isolated CI maintenance: `apelogic-ai/github-oidc-exchange#32` at `13a89de`
  replaces the timing-out emulated ARM smoke with the same check on the native
  `ubuntu-24.04-arm` runner without changing release publication; its native ARM,
  unchanged amd64 quality and Trivy, and Kubernetes replay jobs are green; and
- maintenance merged source: `6d4c106`.
