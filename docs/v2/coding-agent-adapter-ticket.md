# CA-A01: Stable coding-agent adapter contracts

Priority: P1 post-demo

Status: proposed

Implementation owner: Steward team (`apelogic-ai/steward`)

## Goal

Make each supported coding-agent execution protocol a stable Steward adapter so that
compatible product releases can be enabled from the customer-owned GitOps agent catalog
without a new Steward build.

The first additional family is Claude Code under the adapter contract
`claude-code-v1`. The existing `codex-v1` behavior remains unchanged.

The shared release and activation boundary is defined in the
[GitOps coding-agent release lifecycle](coding-agent-release-lifecycle.md).

## Contract boundary

An adapter name identifies execution semantics, not a product release. Steward must
not branch on `agentRef` versions, contain a list of individual Claude Code releases,
or build agent images. Every binding using `claude-code-v1` must satisfy the same
command, configuration, authentication, MCP, output, termination, and diagnostic
contract.

If a future Claude Code release cannot satisfy that contract, operators must not
promote it under `claude-code-v1`. A genuinely incompatible execution protocol uses
a separately reviewed adapter contract such as `claude-code-v2`; existing bindings
and Tasks retain their original semantics.

## Steward scope

- add an `adapters/claude-code` implementation of `TaskExecutionAdapter` with the
  exact contract name `claude-code-v1`;
- render bounded non-interactive execution from the already admitted prompt, one
  approved model, optional governed MCP endpoint, and immutable execution binding;
- use an isolated, run-local Claude configuration directory and disable session
  persistence, automatic updates, ambient MCP discovery, and interactive prompts;
- route inference through a deployment-configured Anthropic-compatible governed
  endpoint using only the OpenShell token-grant placeholder;
- configure only the server-provided MCP-GW endpoint when tools are approved and
  preserve MCP-GW as the tool authorization boundary;
- preserve the common `result.txt` and declared `out/` output contract and the
  existing stdout/stderr diagnostic capture path;
- register both `codex-v1` and `claude-code-v1` in the apiserver;
- allow both contracts in the Helm execution-binding schema and persisted runtime
  validation;
- continue rejecting unknown adapter contracts and bindings whose adapter is not
  registered by the running apiserver; and
- update installation documentation without changing frozen M1/v1.

No TaskDefinition, invocation, Task evidence, CRD, migration, `steward-run`, agent
release catalog, or registry integration change belongs in this ticket.

## Negative proofs

- unknown adapters fail startup or Task submission before reservation;
- a Claude binding cannot select an unconfigured or ambient inference endpoint;
- a tool-bearing Task fails before runtime creation when no governed MCP endpoint or
  tool provider profile exists;
- tool-free execution does not configure an MCP server or token placeholder;
- rendered configuration and command arguments contain no real credential;
- interactive permission or project-trust prompts cannot strand an unattended Task;
- the agent cannot load repository-owned ambient Claude settings, MCP servers,
  plugins, hooks, or session state outside the admitted package inputs; and
- failure and timeout preserve the common Task finalization and durable diagnostic
  behavior.

## Verification

- unit tests cover exact command and configuration rendering for tool-free and
  tool-bearing Tasks;
- negative tests cover missing endpoints, malformed endpoints, credentials in
  rendered material, and unsupported adapters;
- execution-binding chart fixtures cover both adapter names and reject unknown
  names; and
- `cargo xtask ci` passes.

Pinned-binary compatibility and image production belong to CA-P01. Adapter unit tests
must not claim that an arbitrary upstream release conforms.

## Exit criteria

- a released Steward apiserver registers `claude-code-v1` alongside `codex-v1`;
- the adapter consumes only vendor-neutral admitted inputs from `steward-ports`;
- no individual Claude Code version appears in Steward production logic; and
- adding another conforming Claude Code release requires no Steward source change.

## Dependencies and parallel boundary

This ticket is independent of package authorship and GitOps automation. CA-P01 can
implement and prove the generic catalog lifecycle against the existing `codex-v1`
contract in parallel. It can prepare Claude Code image production once this ticket
freezes the `claude-code-v1` command contract. Activating a Claude Code binding waits
for a Steward release containing that adapter.
