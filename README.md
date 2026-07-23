# Steward — documentation and repository scaffold

Extract at the repository root. Everything here is either a working agreement
(`AGENTS.md` and friends) or a specification (`docs/`); no code yet.

## Start here

| If you want to know | Read |
|---|---|
| What Steward is and why | `docs/solution-overview.md` |
| What we are building first, and in what order | `docs/roadmap/steward-roadmap.md` |
| The rules for changing this repository | `AGENTS.md` |

## Layout

```
AGENTS.md                     the working agreement — read before changing anything
CLAUDE.md                     → @AGENTS.md
.gitignore                    matches §11.1, §5, §1.4
deny.toml                     SKETCH — the §8 layering rule, mechanically

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

## What is deliberately not here

- **Code.** The workspace does not exist yet. `deny.toml`'s crate names are
  placeholders and the file is a sketch to verify, not a working gate.
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

## Two things to decide before S0

Both are cheap now and expensive later, and both are in the roadmap's open
decisions table:

- **The API group** (§2.3). Keep the product name out of it — `agents.apelogic.ai`
  — so the pending rename never touches a stored object.
- **`main` vs `master`** (D10). `AGENTS.md` is written against `main`.
