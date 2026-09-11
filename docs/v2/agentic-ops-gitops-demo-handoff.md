# DP-O01: GitOps and Steward UI demo activation

Priority: P0 integration

Status: neutral target-fixture PR `apelogic-ai/gitops#176` merged as `f8ec588`
(reviewed head `e48300b`); final activation PR `apelogic-ai/gitops#177` merged as
`b688f95` (reviewed head `5f4bac1`) with full local repository and GitHub validation
green and no cluster mutation

## Goal

Prepare and activate the retained local-main demonstration without using that
environment for product development or destructive testing. GitOps owns the cluster,
its state, and all deployment changes.

## Inputs expected from implementation tickets

- reviewed merged Steward commit containing DP-G01 and DP-S01;
- reviewed merged Identity commit containing DP-I01;
- immutable `steward-run` reusable-workflow pin containing DP-R01;
- exact `agentic-ops` package commit from DP-A01;
- exact GitHub App configuration and admitted repository IDs;
- package and invocation schemas from DP-C01; and
- the P0 verifier and evidence checklist from DP-P00.

## Work that may start early

- prepare the neutral `apelogic-ai/gitops` caller workflow and checked-in invocation
  manifest with placeholders for reviewed immutable pins;
- prepare strict workflow-dispatch validation for
  `https://github.com/apelogic-ai/gitops/actions/runs/<numeric-run-id>`;
- prepare `in/request.json` and declared output handling;
- inventory the existing read-only GitHub App installation and required repository
  access;
- identify an existing suitable Envelope or document the UI steps to create one;
- prepare preflight checks, evidence capture, rollback, and state-preserving restart
  procedures; and
- prepare source authorization and GitHub connection prerequisites.

These preparations must not deploy unreleased product revisions to local-main or use
the retained cluster to diagnose product implementation defects.

## Final activation

After all reviewed merged commits and exact source commits exist, GitOps:

1. updates deployment and reusable-workflow pins through its normal PR process;
2. installs or confirms the read-only GitHub App on the admitted caller and source
   repositories;
3. configures stable external repository identities and the cross-repository source
   binding;
4. creates or selects the bounded Envelope in Steward UI and approves it;
5. records the UI-approved `steward:sha256` digest in the invocation manifest;
6. confirms the demo user has an active GitHub connection through MCP-GW;
7. reconciles or restarts local-main using only state-preserving GitOps procedures;
8. runs preflight without resetting data;
9. dispatches the end-to-end demo workflow; and
10. retains the required Task, runtime, transcript, provider-call, output, and cleanup
    evidence.

## Required configuration

- Identity accepts only the pinned reusable workflow and preserves verified source
  provenance in its signed exchange JWT; Steward verifies it with the existing direct
  Identity resolver rather than Kubernetes TokenReview.
- Steward trusts only configured Identity issuers and source repository bindings.
- The GitHub source adapter can mint short-lived installation tokens with metadata
  and contents read access only.
- The caller's active Envelope digest matches the invocation manifest.
- The Envelope permits the selected model and the exact read-only GitHub MCP tools
  required by the release-summary prompt.
- OpenShell attaches only desired providers.
- LiteLLM and MCP-GW are healthy and reachable through the governed runtime path.
- Successful-run diagnostic replay is enabled only by the reviewed manifest.

## Verification

- prove exact manifest capture from the verified triggered commit;
- prove cross-repository package retrieval at the declared exact commit;
- prove the recorded closure digest is stable on retry;
- prove Task evidence records external repository identities and exact commits;
- prove a real GitHub MCP call succeeds as the resolved user;
- prove the generated release-summary output is returned to GHA;
- prove stdout and stderr are replayed under labelled GHA groups with the sensitive
  output warning; and
- prove finalization and cleanup complete while retained cluster state remains intact.

## Safety boundary

The local-main cluster is unavailable for implementation testing. Product teams use
their own explicit disposable environments. GitOps alone performs final activation,
preflight, and the demo run. No reset, broad prune, database deletion, namespace
replacement, or unowned-resource deletion is part of this ticket.

## Readiness-audit findings

- Local-main intentionally checks out exact remote `main` commits, builds local OCI
  artifacts, and records immutable sources and digests in
  `main-artifact-lock.json`. Formal product releases are not an additional demo gate.
- The `steward-local-e2e` self-hosted runner is online and idle. GitOps owns the
  `STEWARD_RUNNER_LABEL` variable and workflow use of that label; runner registration
  remains outside this repository.
- Adapt `.github/workflows/steward-governed-release-integration-review.yml` to the v2
  invocation path and add `.steward/tasks/release-summary.json` for the accepted
  cross-repository `gitops` caller to exact-SHA `agentic-ops` package.
- Add a GitHub-hosted, no-auth, no-cluster dispatch fixture under
  `.github/workflows/` so the demo has a neutral repeatable Actions run URL. Existing
  governed and promotion workflows either depend on local-main or mutate state.
- Add request validation that accepts only
  `https://github.com/apelogic-ai/gitops/actions/runs/<numeric-id>` and cover it through
  the repository's normal validation scripts.
- Update the exact reusable-workflow identity in the local Identity policy, its Ruby
  policy generator/tests, and the policy rollout revision after the final
  `steward-run` correction merges.
- Extend the checked-in demo Envelope ceiling by exactly `actions_get`,
  `actions_list`, and `get_job_logs`, then update its revision and mirror assertions.
- Add source-App protected inputs/secret reference, Steward values, caller-to-source
  binding, and apiserver-only GitHub API egress after DP-S01 freezes the chart/API
  names. No key material enters Git.
- The required stable external binding is GitOps repository ID `1318825409` to
  agentic-ops repository ID `1362055860`; App installation and metadata/contents-read
  access must be confirmed on both repositories during preflight.

The GitOps team later runs the normal clean-main validation, state-preserving
`scripts/local-lifecycle.sh main update`, and `main verify`, then checks the artifact
lock, UI-approved Envelope digest, GitHub connection, runner, and App access before
dispatch. No implementation diagnosis or destructive reset occurs on local-main.
