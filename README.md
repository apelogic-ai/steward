# Steward — governance control plane

Steward is a Rust workspace for a self-service agent governance control plane.
The repository implements Slice S0's walking skeleton, Slice S3's envelope
admission path, and Slice S4's escalation path: immutable member-role envelopes,
one shared admission evaluator, the Kubernetes webhook and REST front doors, a
durable approval queue, Jira as an outbound decision channel, and append-only
instance-bound grants.

## Start here

| If you want to know | Read |
|---|---|
| Documentation authority, status, and navigation | [Documentation index](docs/README.md) |
| Normative M1 fields, ownership, and compatibility | [Frozen `steward.m1/v1` contract](docs/contracts/m1/v1/README.md) |
| Accepted post-M1 Agent, Task, session, and runtime semantics | [Post-M1 architecture baseline](docs/v2/README.md) |
| Post-M1 contracts that remain unresolved | [Deferred-contract register](docs/v2/deferred-implementation-contracts.md) |
| Implemented v0.1.x Task lifecycle and identity contract | [Task submission API](docs/task-submission-api.md) |
| Canonical browser / Task person identity | [Canonical user identity](docs/canonical-user-identity-v1.md) |
| Deploy the Task API and worker | [Task deployment](config/task/README.md) |
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
  steward-controller/         Kubernetes controller composition root

crates/
  steward-types/              vendor-neutral shared types
  steward-ports/              eight replaceable-plane interfaces
  steward-admission/          shared admission boundary
  steward-store/              operational history boundary
  steward-controller/         reconciliation and webhook boundary
  steward-apiserver/          REST API boundary
  steward-mint/               protected path; code lands in its own reviewed PR

adapters/
  fake/                       in-memory implementation of every port
  openshell/                  strategic runtime seam
  litellm/ mcp-gw/ jira/
  spire/ opa/                 vendor-plane stubs

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
  task-submission-api.md      lifecycle REST, tar paths, limits, identity boundary
  canonical-user-identity-v1.md
                              immutable user ID, Google OIDC mapping, reconnect contract
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

## What is deliberately not here yet

- **Mint code.** `crates/steward-mint/AGENTS.md` requires changes under that path
  to land in a separate, human-reviewed PR.
- **The identity and budget planes.** S1 and S2 use the recorded OpenShell
  supervisor-identity patch until its upstream exit condition lands. The
  ephemeral identity spike builds that patched supervisor from its immutable
  source revision; S3 and S4 run independently of it.
- **Post-M1 wire contracts.** The accepted semantic boundaries are documented, but
  their unresolved schemas and protocols remain in the
  [deferred-contract register](docs/v2/deferred-implementation-contracts.md).
- **Connector-specific plans.** Burble is the worked example of a frontend connector.
  The accepted boundary is documented in the
  [post-M1 architecture baseline](docs/v2/README.md); its own roadmap and migration
  plan live with the connector, not here.
- **A filled-in push escalation** (`AGENTS.md` §1.3) — left blank on purpose.
  Guessing it produces exactly the retry loop the section prevents. Record it
  the first time someone resolves it.

## Decisions already carried into bootstrap

- **API group:** `agents.apelogic.ai` (§2.3), keeping the working product name
  out of stored objects.
- **Default branch:** `main` (D10).
