# DP-O01: GitOps and Steward UI demo activation

Priority: P0 integration

Status: preparation may start; final activation waits for reviewed component releases

## Goal

Prepare and activate the retained local-main demonstration without using that
environment for product development or destructive testing. GitOps owns the cluster,
its state, and all deployment changes.

## Inputs expected from implementation tickets

- immutable Steward release containing DP-G01 and DP-S01;
- immutable Identity release containing DP-I01;
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

After all reviewed releases and exact source commits exist, GitOps:

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
