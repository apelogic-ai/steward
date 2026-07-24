# Steward — governance control plane

Steward is a Rust workspace for a self-service agent governance control plane.
The repository implements Slice S0 (walking skeleton): an `AgentRuntime` CRD,
controller, and pinned OpenShell adapter provision and tear down a sandbox
idempotently, including across a controller restart.

## Start here

| If you want to know | Read |
|---|---|
| What Steward is and why | `docs/solution-overview.md` |
| What we are building, and in what order | `docs/roadmap/steward-roadmap.md` |
| The rules for changing this repository | `AGENTS.md` |
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
  solution-overview.md        the architecture and the position it takes
  data-plane-spec.md          tiers, gateways, the Tier 1 membership test
  workflow-and-task-spec.md   Plane B object model — Workflow, Task, journal
  steward-ai-workflows-fit.md charter fit; where the gaps are
  guarantee-register-generation.md
                              how the register's status column is derived

  roadmap/
    steward-roadmap.md        stack, slices S0–S5, dependency posture, cadence

  upstream/
    openshell-upstream-strategy.md
    pr-1970-review-comment.md
    rfc-0011-review-comment.md

  design/steward-admin-mockup.html
  diagrams/*.png
```

## Reading order for someone new

1. `docs/solution-overview.md` — the shape of the thing
2. `docs/data-plane-spec.md` §Tier 1 membership test — the rule that decides
   where any new capability belongs
3. `docs/roadmap/steward-roadmap.md` §1 (definition of done) and §6 (the slices)
4. `AGENTS.md` — before touching anything

Plane B (`docs/workflow-and-task-spec.md`) is out of scope for v0.1.0. Read it
for the object model it commits to, not as a build plan.

## What is deliberately not here yet

- **Mint code.** `crates/steward-mint/AGENTS.md` requires changes under that path
  to land in a separate, human-reviewed PR.
- **The identity and budget planes.** S1 and S2 wait for the carried OpenShell
  supervisor-identity fix; S3 and S4 are next in the recorded execution order.
- **`dev-integration-spec.md`** — referenced as a companion by the roadmap but
  not present in this package. Add it if it is still current; the roadmap
  supersedes its sequencing but not its detail.
- **Connector-specific plans.** Burble is named in the roadmap as the worked
  example of a frontend connector (§2.6.7) — an API client that bridges Steward
  to Slack. Its own roadmap and migration plan live with the connector, not
  here.
- **A filled-in push escalation** (`AGENTS.md` §1.3) — left blank on purpose.
  Guessing it produces exactly the retry loop the section prevents. Record it
  the first time someone resolves it.

## Decisions already carried into bootstrap

- **API group:** `agents.apelogic.ai` (§2.3), keeping the working product name
  out of stored objects.
- **Default branch:** `main` (D10).
