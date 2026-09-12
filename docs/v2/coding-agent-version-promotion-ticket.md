# CA-P01: Coding-agent version promotion and execution catalog

Priority: P1 post-demo

Status: proposed

## Goal

Turn a new compatible coding-agent release into an immutable, independently
selectable Steward `agentRef` through artifact promotion and GitOps configuration,
without changing Steward or `steward-run` source.

The first completed promotion proves this lifecycle for Claude Code. The lifecycle
must remain reusable for later Claude Code and Codex versions.

## Ownership

GitOps initially owns:

- exact upstream agent dependency locks;
- agent-capable sandbox image builds and provenance;
- pinned-binary adapter conformance;
- immutable OpenShell provider profiles;
- the deployment execution-binding catalog; and
- activation, rollback, and retirement runbooks.

The existing `steward.execution-bindings/v1` document is the release descriptor
consumed by Steward. Do not introduce another portable Task-package schema or place
image, executable, profile, endpoint, or credential configuration in `agentic-ops`.

## Promotion lifecycle

1. Propose an exact upstream agent version in a reviewed dependency change.
2. Build the admitted platform images from a pinned base and locked dependency set.
3. Scan the images and record their immutable manifest digest and build provenance.
4. Run the adapter conformance lane against the exact executable in that image.
5. Create new immutable tool and inference provider-profile identities and record
   their policy digests. Never mutate an installed profile ID in place.
6. Add a new execution binding containing the exact versioned `agentRef`, adapter,
   image digest, executable, measured version probe, and provider-profile identities.
7. Validate the binding with the released Steward parser and roll it into the target
   environment through the normal GitOps lifecycle.
8. Confirm Steward advertises the new reference while retaining existing supported
   versions.
9. Allow package owners to adopt the new reference independently.

For the first promotion, the currently pinned workflow sandbox may be reused only if
its Claude Code executable and dependency closure pass the required provenance and
conformance checks. A dedicated per-agent image is preferred later because it narrows
the artifact closure and makes independent upgrades cheaper.

## Adapter conformance

The reusable lane accepts an adapter contract, exact image digest, executable, and
version probe. For `claude-code-v1` it must prove, using the real pinned binary:

- exact startup and version output on every admitted architecture;
- unattended non-interactive completion without project trust or permission input;
- inference through the governed Anthropic-compatible gateway with a runtime-scoped
  token grant;
- tool-free execution without MCP attachment;
- tool-bearing execution through only the configured MCP-GW endpoint;
- at least one real allowed MCP call and rejection of a disallowed call;
- required output creation and retrieval;
- bounded stdout/stderr capture for success and failure; and
- timeout, nonzero exit, and missing-output finalization.

The first Claude promotion must also determine which executable OpenShell attributes
network activity to. The npm launcher must not cause a broad Node binary allowance
or another credential-injection bypass to be accepted without an explicit negative
proof. If the existing artifact cannot provide a narrow executable identity, use a
separate native or otherwise isolated Claude image before activation.

## Catalog lifecycle

- `agentRef` is exact and immutable; aliases such as `latest` are forbidden.
- Never reuse an `agentRef` for different image or profile bytes.
- Multiple versions of one agent may coexist in the catalog.
- Adding a conforming version is a GitOps-only catalog addition after artifact
  promotion.
- Removing a binding prevents new submissions for packages selecting it but does not
  rewrite already reserved Task evidence.
- Retain an old binding until package owners have migrated or explicitly accepted
  that new submissions of the old package will fail closed.
- Rollback restores the previous catalog and immutable artifacts; it does not mutate
  a released binding.

## Initial Claude Code activation

- use the exact `claude-code-v1` adapter released by CA-A01;
- select an exact tested Claude Code version, using the version already locked in the
  sandbox only if it passes conformance;
- configure an Anthropic-compatible LiteLLM endpoint and an admitted Claude model;
- add that model to the applicable service and User Envelope setup;
- keep the existing Codex binding active; and
- make no `steward-run` change.

## Exit criteria

- the target environment advertises both an existing Codex reference and one exact
  Claude Code reference;
- the binding and profiles are digest-pinned and independently reproducible;
- the real-binary conformance lane passes on every admitted runner architecture;
- a later conforming Claude Code version can be enabled by dependency, artifact, and
  GitOps changes only; and
- rollback to the previous catalog is documented and verified.

## Dependencies and parallel boundary

Artifact preparation and the conformance harness may proceed alongside CA-A01 after
the `claude-code-v1` command contract is frozen. Catalog activation requires a
released Steward containing CA-A01. Fixed local-main operation remains owned by the
GitOps team, and heavy lanes remain serialized.
