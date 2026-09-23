# Steward — governance control plane

Steward is a Rust/Kubernetes control plane for policy-bound agent workloads.
It provides the `AgentRuntime` CRD and admission webhook, an authenticated API,
a durable approval and audit store, and optional governed Task execution.
The Helm chart supports a staged core installation without Jira, a model
endpoint, OpenShell, LiteLLM, or SPIRE. Governed
execution and Jira are explicit opt-ins with additional prerequisites; they
do not silently activate when an integration is absent.

In Steward v0.2, every external Task is governed solely by the authenticated
user's exact provisioned User Envelope. The controller persists and recovers
that immutable authority snapshot. Product-owned Connection operations use
fixed internal authorities, while the deployment capability catalog describes
available models and tools without granting authority.

Steward is available under the [MIT License](LICENSE). Checked-in upstream
patches retain their [third-party notices](THIRD_PARTY_NOTICES.md).

Current installation contract: chart `0.2.1`, Kubernetes `>=1.30`, Helm 3.17+,
and PostgreSQL 16 as the tested database line. Governed adapter evidence pins
OpenShell `v0.0.98` with agent-sandbox `v0.5.0`. No registry is a default:
release images and the OCI
chart are published under the fork owner's GHCR namespace, and every customer
installation supplies the exact image/chart digests from one handoff. OpenShell
uses the cluster default runtime unless an operator explicitly configures an
optional RuntimeClass; Steward makes no VM-isolation claim. The
[installation guide](docs/installation/installation-guide.md#tested-versions-and-integration-boundaries)
records the limits of this tested matrix.

## Start here

| If you want to know | Read |
|---|---|
| Prerequisites, installation, secrets, post-install checks, and delivery tests | [Installation guide](docs/installation/installation-guide.md) |
| Install order across Steward, `steward-run`, and the identity exchange | [Platform deployment order](docs/installation/platform-deployment-order.md) |
| All chart values and optional integrations | [Helm chart reference](charts/steward/README.md) |
| Documentation authority, status, and navigation | [Documentation index](docs/README.md) |
| Normative M1 fields, ownership, and compatibility | [Frozen `steward.m1/v1` contract](docs/contracts/m1/v1/README.md) |
| Accepted post-M1 Agent, Task, session, and runtime semantics | [Post-M1 architecture baseline](docs/v2/README.md) |
| Post-M1 contracts that remain unresolved | [Deferred-contract register](docs/v2/deferred-implementation-contracts.md) |
| Implemented v0.2 Task lifecycle and identity contract | [Task submission API](docs/task-submission-api.md) |
| Upgrade and rollback boundary from v0.1.23 | [v0.2 upgrade guide](docs/installation/upgrade-v0.2.0.md) |
| Release history and security-relevant changes | [Changelog](CHANGELOG.md) |
| Canonical browser / Task person identity | [Canonical user identity](docs/canonical-user-identity-v1.md) |
| Understand the Task API and worker contract | [Task deployment](config/task/README.md) |
| Configure coding-agent versions | [Execution bindings](docs/installation/execution-bindings.md) |
| The rules for changing this repository | [Agent rules](AGENTS.md) |
| Run the complete local gate | `cargo xtask ci` |

## Layout

```
AGENTS.md                     the working agreement — read before changing anything
CLAUDE.md                     → @AGENTS.md
.gitignore                    matches §11.1, §5, §1.4
Cargo.toml                    Rust workspace
deny.toml                     the §8 layering rule, mechanically

bins/
  steward-apiserver/          API composition root
  steward-controller/         Kubernetes controller and webhook composition root
  steward-mint/               workload credential mint composition root

crates/
  steward-types/              vendor-neutral shared types
  steward-ports/              eight replaceable-plane interfaces
  steward-admission/          shared admission boundary
  steward-store/              operational history boundary
  steward-controller/         reconciliation and webhook boundary
  steward-apiserver/          REST API boundary
  steward-mint/               protected workload-identity path

adapters/
  fake/                       in-memory implementation of every port
  openshell/                  strategic runtime seam
  litellm/ mcp-gw/ jira/
  spire/ opa/                 identity and policy adapters

charts/steward/               Helm deployment, schema, CRD, and chart reference

xtask/                        local and CI gate implementation
policy/                       OPA policy and tests
migrations/                   append-only SQL migrations
manifests/                    generated CRD YAML
e2e/                          external-stack slice exit tests

conformance/
  AGENTS.md                   these tests assert upstream's behaviour, not ours
  register.toml               the guarantee register, declarative half

crates/steward-mint/
  AGENTS.md                   holds the signing key; human review required

docs/
  README.md                     documentation authority, status, and navigation
  contracts/m1/v1/             M1 schemas, fixtures, ownership, compatibility
  v2/README.md                 accepted post-M1 semantics; not a wire contract
  v2/deferred-implementation-contracts.md
                               the sole current post-M1 deferred-contract register
  task-runtime-orchestration.md
                              accepted durable Task runtime state-machine architecture
  task-submission-api.md      lifecycle REST, tar paths, limits, identity boundary
  canonical-user-identity-v1.md
                              immutable user ID, Google OIDC mapping, reconnect contract
  installation/installation-guide.md
                              prerequisites, procedure, and delivery checks
  installation/execution-bindings.md
                              deployment-neutral agent catalog and validation
  m1-delivery-plan.md         historical M1 delivery dependency index
  solution-overview.md       historical long-running-agent exploration
  data-plane-spec.md         historical multi-step data-plane exploration
  workflow-and-task-spec.md  redirect from the superseded Workflow proposal
  steward-ai-workflows-fit.md
                              historical workload-mapping exploration
  guarantee-register-generation.md
                              historical register-generation design note

  roadmap/
    steward-roadmap.md        historical v0.1 roadmap

  upstream/
    openshell-upstream-strategy.md
    pr-1970-review-comment.md
    rfc-0011-review-comment.md

  diagrams/*.png
```

## Reading order for someone new

1. [`docs/README.md`](docs/README.md) — choose the authoritative document for the
   question and understand its status.
2. For M1 integration, read the
   [frozen `steward.m1/v1` contract](docs/contracts/m1/v1/README.md). For accepted
   post-M1 architecture, read [`docs/v2/README.md`](docs/v2/README.md). For current
   implementation, follow the surface-specific documents from the index.
3. [`AGENTS.md`](AGENTS.md) — before touching anything.

The solution overview, data-plane specification, original roadmap, AI-workflow fit
analysis, and Workflow proposal are historical context. They are not current contracts,
implementation status, or committed post-M1 object models. In particular, Steward has
no accepted domain object named `Workflow`.

## Installation boundary

The chart installs Steward resources but does not create a database,
credentials, an issuer, a Gateway, DNS, or an isolation RuntimeClass. An
operator supplies immutable image coordinates and chooses customer-owned TLS
Secrets plus a public webhook CA, or cert-manager with an explicit issuer.
Core mode keeps execution disabled and Task orchestration staged. To enable
governed execution, first validate all dependency and functional sandbox
requirements in the [installation guide](docs/installation/installation-guide.md)
and [execution-binding guide](docs/installation/execution-bindings.md).

Historical design documents remain available through the documentation index;
they are not an installation contract. The API group is
`agents.apelogic.ai`, and the default branch is `main`.
