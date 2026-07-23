# Steward as Substrate for the AI-Workflow Charter

Status: draft — mapping analysis, positions, and two gaps
Audience: ApeLogic engineering
Subject document: *AI-driven workflows — draft plan (what / where / why)*
Companions: *Solution Overview*, *Data Plane Spec*, *Dev & Integration Spec*,
*OpenShell Upstream Strategy*

> **Thesis.** The fit is one-directional. The workflow plan is a **workload catalog**;
> Steward is the **substrate**. Nothing in the plan competes with anything in Steward,
> and three of the plan's own open questions resolve under decisions Steward has already
> made. Two things do not fit, and both are gaps in Steward rather than problems with the
> plan.

> **Terminology note.** This document is written entirely in Steward vocabulary. The
> plan's own terms — *engine*, *adapter* — are translated in §1 and then not used again
> except when quoting. §1.3 also records two vocabulary collisions found while doing the
> translation; both are worth fixing in our own specs.

---

## 1. Terminology

### 1.1 Steward terms used in this document

| Term | Meaning |
|---|---|
| `AgentRuntime` | The manifest / desired-state CRD for one provisioned agent. Governance object: acting user, LLMs, tools, budget, TTL. |
| **Acting user** | The person whose access a runtime is a standing, attenuated delegation of. |
| **Member role** | An organizational role (`engineer`, `data-scientist`) — the key an **envelope** is authored against. |
| **Envelope / ceiling** | The pre-approved limits for a member role that a request is admitted against. |
| **Task** | An admitted plan. Durable, inspectable; nothing enters un-admitted. |
| **Step** | One unit of a task, dispatched by the executor through the step gate. |
| **Step role** | The stable contract for a step's work — `triager`, `checker`, `narrator`. Steward owns the contract. |
| **Role implementation** | The container image that fulfils a step role. Reads a materialized input directory, writes an output directory, exits. This is the process that runs inside the Tier 0 sandbox. |
| **Planner** | The autonomous component that produces a task spec. Distinct from a role implementation: it chooses objectives; role implementations do not. |
| **Handle** | An immutable content identity — commit SHA or blob digest — recorded in the journal. |
| **Journal** | The per-task durable record; the only inter-step channel. |
| **Step gate** | Steward's pre-dispatch check: budget remaining, approval needed, still inside envelope. |
| **Tier 0** | Per-agent non-bypassable enforcement — the OpenShell sandbox with default-deny egress. |
| **Tier 1** | Shared gateways: state no single sandbox can see, custodied credentials, authorization resolved per acting user. |
| **Tool surface** | The set of `provider:resource:action` grants an agent holds at `mcp-gw`. |

### 1.2 Plan term → Steward term

| Plan term | Steward term | Note |
|---|---|---|
| **Engine** (portable) | **Role implementation** | The process inside the Tier 0 sandbox. Portability is a property of the image, not of Steward. |
| **Adapter** (disposable, k0rdent-specific) | *Two different things:* an **ingest role implementation** (e.g. a `normalizer` step), or an **`mcp-gw` tool adapter** | The plan uses one word for both; keep them separate in ours. |
| "One reusable core: a triage engine" | One **step role** contract (`triager`) with three ingest implementations | Aikido findings · CI run logs · OTel events |
| "Normalized CVE + reachability + usage model" | The `triager` role's **input schema**, materialized as a **handle** | This is what "bind to interfaces, not vendors" means operationally |
| "Structural fixes stay deterministic" | A role implementation that makes no model call | Still passes the step gate, still journals |
| Wave 0 / Wave 1 | Two **envelopes** on a member role | §2 — the boundary is an escalation |
| "Ping the review team" | **Parked step** → the existing approval queue | §6 |
| "Where it attaches" | **Tool surface** at `mcp-gw` | |
| "Accumulated judgment — VEX templates, rebuild orders" | The **knowledge layer** (Tier 1) | §7 |
| Syft / Trivy / cosign / SLSA **outputs** | **Handles** recorded in the journal | §5 |
| Distribution as ServiceTemplates | Orthogonal — Steward provisions, Flux/Sveltos distributes | Unchanged by anything here |

### 1.3 Collisions to avoid

Two found while translating. Both are in our own specs, and both are cheap to fix now.

**"Agent" carries three senses in our documents.** Use the specific term:

| Sense | Say instead |
|---|---|
| The provisioned, governed, long-running thing | `AgentRuntime` |
| The autonomous component that produces a plan | **planner** |
| The process running in a sandbox for one step | **role implementation** |

**"Provider" is already taken upstream.** OpenShell's `Provider` is a concrete object
type — credentials, profiles, token exchange (#1970, #2243). Our task-execution diagram
uses "provider" for *"whatever fulfils a step role,"* which is an unrelated meaning. In
specs circulated upstream this will be read the OpenShell way.

> **Action:** rename our usage to **role implementation** in the task-execution diagram
> and spec before it appears in an upstream comment.

**"Role" is overloaded inside Steward itself.** The envelope editor is keyed on
`engineer` / `data-scientist`; the task-execution diagram is keyed on
`architect` / `developer` / `checker` / `verifier`. These are unrelated axes — one scopes
authority, the other scopes work — and a sentence like *"the role's ceiling"* is
ambiguous today.

> **Action:** always qualify — **member role** and **step role**. Never bare "role" in a
> context where both are live.

---

## 2. The waves are envelopes, not phases

**This is the highest-value and lowest-cost thing Steward offers the plan.**

The plan's §1.5 identifies its own #1 risk control as a boundary: *"reads/drafts + no new
env" vs "loop-closers + env-gated"* — and observes that safe and unblocked coincide there.
That boundary is currently held by agreement among the people building the workflows.
Under Steward it is held by the gateway dropping the call.

**Wave 0 is an envelope on a member role, expressible today:**

```yaml
memberRole: ai-workflows-wave0
llms:    [<catalog subset>]
budget:  <$/mo>            # per agent, hard-enforced at LiteLLM
ttl:     720h
tools:
  github:  [read]           # + review comments (write class, non-destructive)
  jira:    [read, write]    # DEVINT — decided
  ci:      [read]           # run logs, artifacts
  aikido:  [read]
  slack:   [write]
destructive: off            # per-call approval required, never pre-granted
```

**Wave 1 is a delta against it**, not a later date:

| Field | Wave 0 | Wave 1 | Consequence |
|---|---|---|---|
| `github` | read | **write** (branches, PRs) | widening → escalates |
| `ci` | read | **trigger** | widening → escalates |
| `registry` | — | **read** (dependency resolution) | new Tier 0 destination |
| env access | none | KSI bare-metal e2e | new tool surface entirely |

Three consequences worth naming:

1. **"Wave 1 starts" becomes an approval with evidence**, in the queue, with the
   blast-radius view showing how many runtimes inherit the widening — rather than a
   decision nobody can point at afterwards.
2. **A Wave-0 workflow cannot drift into Wave 1 by accident.** The plan's §5 notes the
   actuation risk of dependency-bump-with-reasoning *"because it edits callers."* Under a
   Wave-0 envelope that edit is not a policy discussion; it is a denied tool call.
3. **The anti-ratchet invariant applies unchanged.** Admission evaluates absolute
   requested values, so a sequence of individually small widenings cannot walk Wave 0 into
   Wave 1.

**Position: author both envelopes now.** Wave 1 existing as an unapproved envelope is
free and makes the boundary legible; approving it is the phase transition.

---

## 3. Every workflow is a task shape

The plan's workflows map onto the existing plan → admit → journal → governed-step model
with no new machinery. Step roles are the plan's engines; the tool surface is where its
adapters attach.

| Workflow | Step roles | Tool surface | Deliverable → system of record | Wave |
|---|---|---|---|---|
| Release-readiness | collector → differ → narrator | `github:read`, `jira:read/write` | gap report → Jira / Doc | 0 |
| CVE triage + priority-by-usage | normalizer → **triager** → justifier | `aikido:read`, graph handle, `jira:write`, `slack:write` | prioritized queue + VEX draft → Jira | 0 |
| Flake-vs-regression | normalizer → **triager** → deduper | `ci:read`, `jira:read/write` | issue filed / updated → Jira | 0 |
| Build + CVE status summary | collector → summarizer | `ci:read`, `slack:write` | digest → Slack | 0 |
| Release notes / changelog | collector → writer | `github:read` | draft → PR / release body | 0 |
| Security PR reviewer | reviewer | `github:read` + review comment | review comment → PR | 0 |
| Remediation-to-green loop | **architect → developer → checker → verifier** | `github:write`, `ci:trigger`, registry | repackage PR → GitHub | 1 |
| Dependency-bump-with-reasoning | developer → checker (bounded loop) | `github:write`, `ci:trigger`, registry | patch PR → GitHub | 1 |
| Unified build triggering | ops | tag triggers, `ci:trigger` | pipeline run | 1 |

Three observations:

- **The `triager` step role appears three times**, exactly as the plan predicts, with
  three ingest implementations. That is one role contract to write, and the plan's
  reusability claim is structurally true rather than aspirational.
- **The remediation-to-green loop is the existing architect/developer/checker/verifier
  chain**, unchanged, including the bounded `max N` retry that escalates rather than
  spinning. Third independent convergence in this project.
- **Every remediation workflow terminates in a PR** — the publish-once shape from the data
  plane spec, with the gateway publishing and the commit attributed to the acting user's
  agent.

**Deterministic steps stay deterministic.** The NICO-only prune and the distroless base
swap are steps whose role implementation makes no model call. They pass through the same
step gate, journal, and admission, and hold no credentials — which restates the plan's
§1.4 as an execution property rather than a discipline.

---

## 4. The graph / usage-signal fork, resolved

The plan calls this its single hard-to-reverse decision (§5, §6) and frames it as
**batch vs interactive**. Under the Tier 1 membership test it is a cleaner question, and
it resolves differently.

| Option | Owns state no sandbox can see | Custodies a credential | Resolves authz per acting user | Verdict |
|---|---|---|---|---|
| **Graph as build-time artifact** | no — it is content | no | no — digest-only reachability | **Tier 0.5**, alongside SBOM / provenance |
| **Graph as live service** | yes — cross-repo | yes | **must**, and cross-repo is the hard case | **Tier 1**, the hard kind |

The service option is not merely more infrastructure. A call graph spanning repos with
mixed entitlement is the **declassification hazard verbatim**: a query returns facts
derived from repos the acting user may not read, and the answer cannot be diffed against
its sources to prove what leaked. That drags the knowledge layer's labeling problem into
Wave 0 — precisely where the plan wanted its safest work.

**The argument the plan does not make: replayability.**

The output of CVE triage is audit material. *"Why did we deprioritize CVE-X in March"* is
answerable only if the graph it was judged against is pinned. A handle recorded in the
journal answers it exactly; a live service cannot, because the graph has moved. For a
workflow whose deliverable is a VEX justification intended to pass audit, that is close to
decisive on its own.

> **Position.** The artifact is **authoritative**; a service, if built, is a **read-only
> convenience view**. Decisions cite handles. Humans browse the service. This buys
> interactivity without moving the provenance problem into Wave 0, and it keeps the fork
> reversible in the direction that matters — a service can be added later over the same
> artifacts; artifacts cannot be reconstructed from a service after the fact.

Practical shape: the graph is emitted alongside the SBOM, its handle recorded per task,
resolved through the blob CAS. Same mechanism as every other artifact in the data plane.

---

## 5. Trust anchors are already the handle contract

The plan's §3 binds to the **outputs** of Syft/Trivy, cosign/sigstore, SLSA, ko/Bazel —
never the tools. In Steward those outputs are handles recorded in the journal, which
yields the plan's stated safety property directly:

> *"A wrong AI judgment is bounded and auditable because provenance can prove exactly
> which build runs."*

The journal already pins which inputs a judgment was made against, and the step gate
already sits in the dispatch path. So the auditability the plan wants from provenance
tooling composes with the auditability Steward provides for the reasoning itself. Neither
was designed for the other.

---

## 6. The metric lands in the approval queue — if the workflows cooperate

The plan is right that the bottleneck relocates rather than vanishing, and right about the
metric: **review-time-per-assessment and accept/sign-off rate**, not assessments-drafted.

Under Steward that is **queue telemetry** — measured for free, per workflow, per reviewer,
from parked-step timestamps already recorded. But only under one constraint:

> **Constraint.** Every human gate in every workflow routes to the **one** approval queue.
> A workflow that grows its own Slack-thread approval, or its own GitHub review-request
> convention, is unmeasurable and re-fragments the authority model.

Cheap to state now, expensive to retrofit once six workflows have shipped with bespoke
gates. It is the "one queue for every escalation" property already in the task-execution
design, applied to a new class of workload.

Corollary worth taking: **accept/sign-off rate per step role** is the signal for whether a
given role implementation is ready to widen. That makes the Wave 0 → Wave 1 approval
evidence-backed rather than calendar-driven.

---

## 7. Their moat is our unbuilt block

The plan's §5 is unusually honest: portability is not a moat, and durable value accrues in
*"integration quality and accumulated judgment — VEX templates that actually pass audit,
rebuild orders that are actually correct."*

That accumulated judgment is **the knowledge layer**. The plan's stated source of durable
value is therefore the one Tier 1 block with no design yet — and it arrives with the
labeling problem attached:

- A VEX justification synthesized across repos with mixed entitlement carries **every
  source's label**.
- Cross-project rebuild-order knowledge unions its label sets, narrowing its audience —
  correct, and also why cross-project knowledge is inert until someone declassifies it.
- **Declassification is an approval action**, evidenced, in the same queue.

**This raises the priority of the knowledge layer's labeling granularity decision (D6 in
the data plane spec).** It was schema-level and hard to retrofit; it is now also on the
critical path of what the workflow charter is betting on.

---

## 8. Gap 1 — unattended workloads have no principal

**A real gap in Steward, not a wrinkle in the plan.**

Steward's model is that an `AgentRuntime` is a standing delegation of *a person's* access,
attenuated — never a superset. The email join key, the identity mint, `mcp-gw`'s per-user
token resolution, and the teams model all rest on a non-null acting user.

Several of the plan's workflows have no such person:

| Workflow | Acting user | Problem |
|---|---|---|
| Release-readiness | Leo, on demand | fine — the delegation is real |
| Security PR reviewer, per PR | author? reviewer? | ambiguous, arguably fine |
| **Scheduled org-wide CVE scan** | **nobody** | not attenuated from any person |
| **Build + CVE status summarization** | **nobody** | runs on a timer across all pipelines |
| **Unified build triggering on tags** | **nobody** | triggered by an external event |

Naming a human acting user on an org-wide scan is a fiction, and the fiction is
load-bearing: it would make one person's envelope the ceiling for org-wide work, and it
would attribute org-wide actions to someone who did not take them.

**Position: a second principal class — the service principal.**

- Envelope **authored directly** by an admin, not derived from a person's access.
- Same admission, same escalation path, same spend accounting, same queue.
- Has a **named human owner** for accountability and notification — owner ≠ acting user,
  and the distinction must be explicit in the CRD rather than implied.
- Cannot resolve per-user provider credentials at `mcp-gw`; needs its own custodied
  credential with an explicitly authored scope. **This is the part that must not be
  quietly widened** — a service principal with org-wide read is the lateral-movement
  primitive again.
- Attribution on output is *"the CVE-scan agent, owned by \<team\>"* — never a person.

Same shape as **#1757** upstream (always-on agents with no active session), and worth
saying so in that thread: we operate the case they describe.

**Cost of deferring is high** — `spec.actingUser` being assumed non-null propagates into
the mint, the projections, and the teams model.

---

## 9. Gap 2 — Steward must not absorb role implementations

The plan's portability hedge is deliberate: its engines port to any container shop, and
that portability is the hedge *"if the roadmap is built in-house or the relationship
cools."* In our terms: **role implementations must remain ordinary container images.**

Steward can destroy that property without anyone deciding to. Each of these looks locally
reasonable:

| Creeping coupling | What it costs |
|---|---|
| Implementation resolves its own input by calling Steward for a handle | needs Steward to start |
| Implementation writes its output as a journal entry | needs Steward to finish |
| Implementation consults the step gate before acting | policy logic has leaked inside |
| Implementation calls tools directly with its HOP-1 token | its auth model is now Steward's |
| Input schema drifts from the normalized model toward Steward's task schema | the interface it was meant to bind to is gone |
| Implementation handles its own parking on approval | it now knows what an approval is |

Any three and the image does nothing useful on its own. Nobody made that decision; it
accretes.

> **Rule.** A role implementation is a **pure function over a directory**. Inputs are
> materialized into the filesystem before it starts; it reads files, writes files, exits.
> It never resolves a handle, never holds a credential, never asks whether it is allowed.
> Steward does handle resolution, credential injection, and gating **around** the process,
> never inside it.

This costs nothing architecturally — it is the containment property the Tier 0 sandbox
already provides. It is a discipline about where code goes.

**Make it a CI check, not a principle.** One test per role implementation, executing it
with no Steward present:

```bash
docker run --rm --network=none \
  -v ./fixtures/cve-triage/in:/in:ro \
  -v "$OUT":/out \
  -e INFERENCE_BASE_URL=http://stub:8080/v1 \
  triager:"$TAG"
diff -r "$OUT" ./fixtures/cve-triage/golden
```

The input is a fixture directory and a manifest in which the handle is an opaque string
the implementation only copies into its provenance output.

**The one permitted dependency:** model access, injected as an OpenAI-compatible base URL
plus a key — LiteLLM in production, a recorded-fixture stub in CI. That is an open
interface, and specifically not a Steward dependency.

**Why this is our interest, not only theirs.** Portable implementations mean Steward
competes on being good at governance rather than on being expensive to leave. They are
also independently testable, swappable per workload, and iterable without a Steward
environment — the difference between a fast inner loop and a slow one.

---

## 10. What this adds to the block inventory

| Block | Change | Driver |
|---|---|---|
| Package registry proxy | **priority confirmed** — Wave 1 dependency work is dead without it | plan Wave 1 |
| Blob CAS | now carries SBOM, provenance, **and the usage graph** | §4 |
| Knowledge layer | **priority raised** — it is the plan's stated moat | §7 |
| Fetch proxy | advisory sources (NVD, GHSA, vendor bulletins) are a read surface with real injection risk | CVE triage |
| Notification egress | Slack alerting appears in two Wave 0 workflows; attribution must be *agent-of* from day one | plan Wave 0 |
| `mcp-gw` | new tool adapters: Aikido, CI, Jira DEVINT — all read-heavy, all Wave 0 | plan Wave 0 |

No new tier, no new boundary, no new policy engine.

---

## 11. Open decisions

| # | Decision | Position | Cost of deferring |
|---|---|---|---|
| A1 | Wave 0 / Wave 1 as authored envelopes | author both now; approving Wave 1 *is* the phase transition | low, high payoff |
| A2 | Graph as artifact vs service | **artifact authoritative**, service is a read-only view | **high** — the plan's own hard-to-reverse call |
| A3 | Service principal class | add before `actingUser` is assumed non-null | **high** — retrofit touches mint, projections, teams |
| A4 | One approval queue for every human gate | mandatory constraint | **high** — unmeasurable and fragmenting once shipped |
| A5 | Role-implementation portability CI check | one test per image, runs outside Steward | medium — coupling is gradual and hard to reverse |
| A6 | Knowledge-layer labeling granularity (D6) | still undecided; now on the critical path | **high** — schema-level |
| A7 | Deterministic steps as role implementations with no model call | yes — keeps the journal complete | low |
| A8 | Security PR reviewer's acting user (author / reviewer / service principal) | undecided; likely service principal with repo-scoped read | medium |
| A9 | Rename "provider" → "role implementation"; qualify member role vs step role | do it before the next upstream comment | low now, confusing later |

---

## 12. Two notes for the upstream engagement

Both are evidence, which is what the OpenShell strategy says converts to standing.

1. **#1757 (standing delegation for unattended agents)** — §8 is the operating case that
   issue describes, with a concrete taxonomy of which workloads have a delegator and which
   do not. Comment on the issue; do not re-file.
2. **#2109 / PR #2168 (managed maximum policies)** — the Wave 0 → Wave 1 transition is a
   live example of *"a granted exception must bind to the instance, not widen the
   maximum"* and of the parked-request-with-structured-delta gap. Lead with what we
   operated, per the standing rule about abstract architecture posts getting silence.

---

## 13. Summary position

- The plan needs no changes to fit. It is a workload catalog; Steward already has the
  execution model for it.
- The waves should be authored as envelopes immediately — the cheapest conversion of a
  discipline into an enforcement available anywhere in this engagement.
- The graph fork resolves to **artifact authoritative**, on a replayability argument the
  plan had not made.
- Two gaps are ours: **service principals** for unattended workloads, and a **portability
  guard** so Steward never absorbs role implementations.
- The plan's stated moat is the knowledge layer, which raises its labeling decision from
  "hard to retrofit" to "on the critical path."
- Three vocabulary fixes fall out of the translation (§1.3) and are cheapest now.
