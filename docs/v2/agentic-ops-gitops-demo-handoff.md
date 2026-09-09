# P0 agentic-ops demo: GitOps handoff

Status: repository preparation complete; live publication and rehearsal unverified.
This is a deployment/readiness handoff, not permission for development workers to
operate or test the GitOps-managed local-main cluster.

## Reviewed source and caller

| Item | Prepared value |
| --- | --- |
| Repository | apelogic-ai/agentic-ops, private |
| Repository / owner IDs | 1362055860 / 227278099 |
| Human-created default main | 268fb6f29779ba145510f54135902c15d0942af9 |
| Caller | .github/workflows/steward-task.yml, main, workflow_dispatch only |
| Reusable steward-run workflow | a86a227f0f731a8a96628d4628f03b4482ad612b |
| Nested steward-run action | 0707623836cd4cdf063938e1e049c694397bc31c |
| Legacy published coordinate | agentic-release-integration-review@1 |
| Agent binding | codex@0.117.0 |
| Expected content digest | sha256:9b7e9bf82754b159b3cb1bff72a898a8eb1f0308c1792e283032fbebd4098aaa |
| Package byte digest | sha256:467705ecb9ebbd9a25504bf87a1fb8f0931bdc7664d378ebd470935d9260a63b |
| Dedicated runner label | steward-agentic-ops-main |

The repository's immutable OIDC subject setting is enabled. Its non-secret runner,
Steward URL, Identity URL/audience, and host CA-reference variables are configured;
the host operator must verify them against the actual prepared demo environment.
No runner was registered or started by this workstream.

[GitOps #164](https://github.com/apelogic-ai/gitops/pull/164) merged the separately
authorized enforcement-only change. [GitOps #165](https://github.com/apelogic-ai/gitops/pull/165)
adds only the exact task caller and the local-v5 Identity rollout revision. Human
review/merge and GitOps-owned rollout are separate from local validation of that PR.
No bootstrap, actor, Envelope, existing caller, or branch-protection changes are included.

## GitOps and operator preparation

1. Select and record the compatible approved component revisions and image/chart
   digests. Verify actual deployment against them; do not equate a retained dirty
   worktree record with a tested #78 release. User updates about #78 are not rollout
   authority. GitOps owns cluster recovery, deployment, and readiness.
2. After the policy PR is human-merged, GitOps rolls out the exact Identity mapping
   through its supported lifecycle. No development tests or ad hoc changes use
   local-main. Stable promotion requires its own approved release and handoff.
3. The host operator prepares a dedicated runner with an appropriate service identity,
   protected files, no ambient infrastructure credentials, and serialized access to
   the demo host. A prepared, offline-tested hook pins the exact source above; it is
   not installed and is not filesystem isolation or a cross-runner concurrency lock.
   Do not add runner management to the GitOps local lifecycle scripts.
4. Verify the canonical user, approved provisioned Envelope, real model, native
   ARM64 execution binding, and usable private GitHub connection. The requested task
   needs get_file_contents:read. Existing user connections are not disposable Task
   resources and must not be revoked as Task cleanup.
5. An authorized publisher uses the package's validated legacy publication request.
   Read back agent, prompt, coordinate/version, and digest. Version 1 is not yet
   published by this workstream; if it already exists with different content, stop
   and prepare a new immutable version rather than overwrite it.

## Agreed rehearsal window

Run two ordinary GHA dispatches against unchanged reviewed pins only after GitOps
confirms readiness and agrees the window. The release scenario is synthetic; model
inference, governed source access, runtime execution, and cleanup must be real.

For each run, capture the exact source commit, GHA run/attempt, Task/runtime UID,
useful summary, terminal state, finalization, and sanitized independent evidence:

- The hosted verifier checks an unpredictable marker read from the private README
  at the source commit. Keep the expected marker outside the Task's prompt, inputs,
  uploaded files, environment, checkout, and earlier context. Verify the sandbox has
  no alternate source-access credential or path. This proves source retrieval under
  those conditions, not a signed MCP call or a future catalog witness.
- Corroborate tool execution with the native MCP completed event in the exact
  UID-bound execution stream. The [pinned Codex human-mode event](https://github.com/openai/codex/blob/4c70bff480af37b1bf1a9b352b8341060fe55755/codex-rs/exec/src/event_processor_with_human_output.rs)
  reflects the typed tool outcome, but text has no authenticated request identity
  or argument binding.
  Do not manufacture request IDs or substitute an agent-authored VERIFIED line.
- GitOps chooses a safe, deployed-version-compatible collector for successful real
  inference records correlated to the runtime key. Aggregate spend alone is not
  request-level evidence. Do not export raw key objects, provider bodies, or logs.
- Confirm exact disposable runtime/sandbox/Secret absence and runtime model-key
  cleanup. These resources may occupy different namespaces. Shared provider
  definitions and pre-existing user OAuth connections need not disappear. Record
  any authority-revocation property that lacks a direct observation as unverified.

Full task-I/O logs can contain sensitive material; GitOps must project only the
necessary non-secret evidence before saving or sharing it. Missing evidence is a
recorded gap, not a passing assertion. Formal Ticket B request-level conformance is
not replaced by this narrower attended demonstration.

Keep an owner, purpose, retention deadline, and exact scoped cleanup action for
every retained demo service, runner, credential, and connection. Do not remove
unrelated state. A later main commit requires source review, deliberate hook repin,
and a new unchanged-pin rehearsal pair.

## Development work remains separate

- The P0 package has 19 passing local tests and successful source-validation CI;
  this is not a successful governed Task receipt.
- [Agentic-ops #1](https://github.com/apelogic-ai/agentic-ops/pull/1) contains optional
  offline publication/provenance tooling, not a live publisher. Hold unrelated main
  changes during the chosen demo rehearsal pair.
- Ticket B's checker is checkpointed locally. Its full gate failed in an isolated,
  run-owned G-1 setup on SPIRE/etcd timeouts before the security assertion; cleanup
  was verified. The remaining pinned tests and live conformance are not passing claims.

See the [P0 ticket](agentic-ops-local-demo-ticket.md) for the customer outcome and
the package's demo setup guide for the publication and output contracts.
