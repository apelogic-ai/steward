# DP-P00: Direct-package governed GHA local demo

Priority: P0

Status: approved architecture; implementation pending

## Goal

Demonstrate a customer-representative operating model in which a platform engineer
authors a pre-reviewed Task package in `apelogic-ai/agentic-ops`, a normal workflow in
`apelogic-ai/gitops` invokes its exact Git identity, and Steward executes it under an
administrator-approved Envelope using real GitHub MCP access.

No approval ceremony occurs during the presentation. The package, invocation manifest,
source authorization, GitHub connection, and Envelope are prepared beforehand.

## Dependencies

- PR 78 common Task application and orchestration core: complete.
- DP-C01 direct-package contracts.
- DP-I01 verified GitHub source provenance.
- DP-G01 provider-neutral Git source port and GitHub adapter.
- DP-S01 direct-package Steward resolution and execution.
- DP-R01 `steward-run` direct invocation and successful-run transcript.
- DP-A01 release-summary package.
- DP-O01 GitOps and UI activation.

## Demo fixture

- caller: `apelogic-ai/gitops` at the exact workflow-trigger commit;
- source: `apelogic-ai/agentic-ops` at an exact `git:sha1` commit;
- package path: `catalog/release-summary/v1/task-definition.json`;
- package behavior: one prompt, no skills, omitted `requires`;
- input: a workflow-dispatch run URL belonging to `apelogic-ai/gitops`;
- Envelope: approved in Steward UI and referenced by `steward:sha256` digest;
- provider path: GitHub MCP through MCP-GW with real read-only calls;
- result: `out/release-summary-<run-id>.md`; and
- diagnostics: successful agent stdout and stderr replayed in GHA after a sensitive
  output warning.

## Presentation flow

1. Show the reviewed package and invocation manifest in Git.
2. Show the approved Envelope in Steward UI and confirm the digest matches the
   manifest.
3. Dispatch the ordinary `apelogic-ai/gitops` workflow with a valid run URL.
4. Show Identity-authenticated source resolution and Task creation.
5. Show the exact package repository, commit, path, and closure digest in evidence.
6. Show runtime execution under the approved Envelope.
7. Show real GitHub MCP activity in the successful-run transcript.
8. Show the generated release summary and Task/runtime correlation identifiers.
9. Show successful finalization and cleanup.

## Required negative proofs

- a package commit that does not match the resolved Git object is rejected before
  Task reservation;
- an unauthorized cross-repository source is rejected before package content is
  trusted;
- an absent or non-active Envelope digest is rejected;
- a workflow token or caller-supplied Git credential cannot replace the GitHub App
  source boundary;
- a run URL outside the allowed neutral fixture repository is rejected by the caller
  workflow; and
- the E2E verifier fails if no real GitHub MCP call occurred.

## Exit criteria

- all dependency tickets are merged and released at reviewed immutable versions;
- GitOps performs a state-preserving activation on its owned local-main environment;
- the preflight proves Identity, Steward, OpenShell, LiteLLM, MCP-GW, source access,
  user GitHub connection, and the approved Envelope are ready;
- the governed workflow succeeds from a clean dispatch;
- the output summary contains only observed fixture data;
- the GHA log contains the successful-run stdout/stderr transcript and evidence of a
  real GitHub MCP call;
- Task evidence binds source, closure, authority, Principal, runtime, input, and
  output identities; and
- cleanup completes without deleting or resetting retained GitOps-owned state.

## Explicitly out of P0

- AgentSession;
- DEV readiness;
- live authorship or publication approval demonstrations;
- mandatory catalog publication, tags, or GitHub releases;
- multiple workflows or packages;
- over-Envelope approval demonstrations;
- stable-lane testing beyond the single demo proof;
- executable skills; and
- durable logs for failed or cancelled Tasks.
