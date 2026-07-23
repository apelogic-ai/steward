# Workflow and Task — the Missing Object

Status: draft — proposes two new objects and one change to admission
Audience: ApeLogic engineering
Companions: *Solution Overview*, *Dev & Integration Spec*, *Data Plane Spec*,
*AI-Workflow Charter Fit*, *OpenShell Upstream Strategy*

> **Thesis.** Steward has an `AgentRuntime` (who may act, with what authority) and a task
> (what is being done right now), and nothing in between. The recurring *shape* of the
> work — the coding loop, the package-tree review, the marketing pipeline — is currently
> implicit in the planner's output, which means every task is bespoke, nothing can be
> approved by shape, and no authority question can be answered before a task runs.
>
> **The fix: the workflow is an envelope, not a program.** Everything else in Steward is
> authored once and enforced many times. The shape of the work should be too.

---

## 1. Two objects

| Object | Is | Analogy |
|---|---|---|
| **`Workflow`** | The recurring shape. Authored, versioned, reviewable. Declares which task shapes are legal. | Grammar |
| **`Task`** | One admitted walk through a workflow. What the executor runs. | Sentence |

The planner emits a sentence; admission checks it is **in the language** *and* **within
authority**. Those are two separate checks and both are new:

- **In the language** — every node the plan uses is declared by the workflow; every
  transition is a declared edge; every cycle is within its declared bound.
- **Within authority** — the composed plan sits inside the member role's envelope, as
  today.

Today only the second check exists, and it runs against an unconstrained plan.

---

## 2. What a `Workflow` declares

```yaml
apiVersion: steward.apelogic.ai/v1alpha1
kind: Workflow
metadata:
  name: coding-loop
spec:
  revision: 7                    # immutable once published

  planner:                       # optional — see §6
    role: planner
    tools: [github:read, jira:read]

  nodes:
    - name: architect
      role: architect            # step role, resolved via the role catalog (§7)
      tools: [github:read, jira:read]
    - name: developer
      role: developer
      tools: [github:read, registry:read]
    - name: checker
      role: checker
      tools: [github:read, ci:trigger]
    - name: publish
      role: publisher
      tools: [github:write]      # the ONLY node holding write

  edges:
    - {from: architect, to: developer}
    - {from: developer, to: checker}
    - {from: checker,   to: developer, maxTraversals: 3}   # static cycle bound
    - {from: checker,   to: publish}

  io:                            # handle contract between steps
    - {from: architect, to: developer, handleType: task-repo}
    - {from: developer, to: checker,   handleType: task-repo}
    - {from: checker,   to: publish,   handleType: task-repo}

  gates:
    - {node: publish, requires: human-approval}

  deliverable:
    kind: pull-request
    systemOfRecord: github

status:                          # computed by the controller, never authored
  authorityBound:
    tools: [github:read, github:write, jira:read, registry:read, ci:trigger]
    maxSteps: 14
    worstCaseSpend: "<derived>"
  digest: sha256:…
```

Five declarations do the work:

1. **Nodes** — the step roles this workflow may use, **each with its own tool surface**.
   This is per-step attenuation, not per-task: `checker` gets `github:read`, only
   `publish` gets `github:write`.
2. **Edges** — permitted transitions, cycles allowed, but **every cycle carries a static
   `maxTraversals`**. The existing developer↔checker `max N` loop is already this.
3. **`io`** — the handle type flowing across each edge. The data-plane contract, declared
   rather than assumed.
4. **`gates`** — where a step parks for human approval.
5. **`deliverable`** — the terminal artifact and its system of record.

---

## 3. The property that justifies the object: a static authority bound

Union the tool surfaces over all reachable nodes and you know what this workflow can
**ever** do, before it runs.

```
authorityBound.tools = ⋃ { node.tools | node reachable from an entry node }
```

Because cycles are bounded, the reachable-walk length is finite, which gives two more
static facts:

- **`maxSteps`** — the longest legal walk.
- **`worstCaseSpend`** — `maxSteps` × per-step ceiling, computable before admission.

This is what makes **approval by shape** possible. *"The marketing pipeline can never
touch GitHub"* becomes checkable rather than promised. Without the object you can only
gate step-by-step at runtime, and you can never answer the question an approver actually
asks, which is not *"is this step allowed"* but *"what is the worst this can do."*

`worstCaseSpend` is worth more than it looks: today budget exhaustion is discovered at
runtime and drives the Running → Suspended transition. With a bound it is knowable at
admission, so a task that *cannot* complete inside the remaining budget can be rejected
with a structured delta instead of dying halfway and leaving a partial deliverable.

**Upstream note.** This is the #2109 contract one level down — *candidate effective
authority ≤ applicable managed policy envelope* — applied to plan shape rather than
sandbox policy. Worth raising there; their subagent expansion (#2025) is asking exactly
this question about containment chains.

---

## 4. Three envelopes, intersected

```
effective authority  =  member role envelope
                        ∩  workflow authority bound
                        ∩  the task's admitted walk
```

**Intersection, never union.** Leo's envelope says what his agents may ever do; the
workflow says what this shape may ever do; neither widens the other. This is the same
trap avoided with team membership — a person in two teams does not get the union of two
envelopes.

Consequences for admission:

| Case | Outcome |
|---|---|
| Plan uses a node the workflow does not declare | reject — not in the language, no escalation path |
| Plan exceeds a declared cycle bound | reject — not in the language |
| Workflow bound exceeds the member role envelope | escalate — the normal widening path, with deltas |
| Walk is legal and inside both envelopes | admit |

Note the first two are **rejections, not escalations.** A malformed plan is not a request
for more authority; it is a plan that does not typecheck. Keeping those outcomes distinct
matters — otherwise every planner bug lands in the approval queue.

---

## 5. Declarative, and deliberately not Turing-complete

**Forbidden in a workflow definition:**

- dynamic step construction (nodes computed at runtime)
- computed or templated tool names
- unbounded loops or recursion
- conditional edges predicated on arbitrary expressions

The moment the definition is a program, the reachable set is undecidable and the authority
bound evaporates — taking with it approval-by-shape, `worstCaseSpend`, and every claim in
§3. Same reason policy is Rego rather than Python, and the prover already wants analyzable
input.

**Permitted:** parameters (repo, ticket, target branch), conditional edges over a *closed*
enumerated set of step outcomes (`pass` / `fail` / `needs-human`), and per-node parameter
binding. Conditions select among declared edges; they never create edges.

The test for any proposed feature: **can the controller still compute the reachable node
set statically?** If not, it does not go in.

---

## 6. The planner is a step, and most workflows do not need one

The planner is an ordinary node — usually read-only — whose output must be a valid walk
through its own workflow. It is not privileged and holds no special authority.

This makes static pipelines the **degenerate case** rather than a separate mechanism:

| Workflow | Planner | Why |
|---|---|---|
| `coding-loop` | yes | step count varies with the work; the grammar generates many sentences |
| `package-tree-review` | no | grammar generates one sentence, parameterized by tree root |
| `marketing-pipeline` | no | fixed stages; parameters vary, shape does not |
| `cve-triage` | no | normalizer → triager → justifier, always |

So *"has a planner"* is a property of the workflow, not a property of Steward. Most of the
AI-workflow charter's Wave 0 is plannerless — which also means most of it has a trivially
verifiable authority bound and a single legal walk.

---

## 7. The role catalog — a second registry

The workflow references **step roles**, not images. Something must resolve the reference.

```yaml
kind: RoleCatalogEntry
metadata:
  name: triager
spec:
  inputSchema:  <ref>            # the normalized model
  outputSchema: <ref>
  implementations:
    - image: registry.internal/triager
      digest: sha256:…           # pinned, never a tag
      attestation: <provenance ref>
      default: true
```

Why the indirection is load-bearing:

- **"Which implementation, which version" stays a separately governed decision** from
  "what shape is this work." Swapping a triager implementation does not edit a workflow.
- It is where the **supply-chain story lands** — digest pinning and provenance
  attestation, which is exactly what the charter's deterministic substrate
  (cosign/SLSA/Syft) is for.
- It enforces the portability rule: an entry declares an I/O contract, so an
  implementation that needs Steward to run cannot satisfy it.

---

## 8. Creation — three paths, one artifact

| Path | When | Note |
|---|---|---|
| **Authored** | stable, known pipelines | hand-written YAML, reviewed like policy |
| **Promoted from a successful run** | shape discovery | run it loose once under a permissive envelope, freeze what worked into a definition |
| **Generated and pinned** | bootstrapping a new shape | generated by a model, then reviewed and frozen — generation is never live |

Promotion is the interesting one and probably the most-used. It is how shapes are actually
discovered: nobody authors the right coding loop on the first try. The promotion tool
takes a completed task's journal and emits a candidate `Workflow` whose nodes are the
roles that ran, whose edges are the transitions taken, and whose cycle bounds are the
observed maxima plus headroom. A human reviews the diff before it is published.

**All three land in the same object.** There is no "ad-hoc mode" that bypasses the type.

---

## 9. Storage, versioning, pinning

- **A Kubernetes CRD**, alongside `AgentRuntime`. Kubernetes-native audit and RBAC for
  free, and GitOps-able on the Flux/Sveltos road already in place.
- **Revisions are immutable.** Editing publishes a new revision; it never mutates one.
- **Content-addressed.** The controller computes a digest over the spec.
- **The task pins the digest it was admitted against.** Same replayability argument as the
  usage-graph fork: if the definition can mutate mid-run, replay means nothing and
  *"why was this admitted"* is unanswerable.
- **Retention follows the journal.** A workflow revision cannot be garbage-collected while
  any retained journal cites it.

---

## 10. Editing a workflow is a widening change

If a new revision's `authorityBound` is a superset of the previous one's, **it goes through
the same door as any other widening**: structured deltas, approval queue, evidence link,
blast-radius view showing how many runtimes and scheduled tasks inherit it.

If the bound narrows or is unchanged, it applies immediately — the same asymmetry already
in the envelope editor.

This is a genuine security win and the least obvious benefit of the object. Today, adding
a tool to a step is a code change nobody reviews as a policy change. With a definition it
is a policy change with a computed delta:

```
coding-loop rev 7 → rev 8
  + tools: github:write on node `developer`
  authorityBound: unchanged (publish already held it)
  BUT: reachable-with-write node count 1 → 2
```

That second line is the one a reviewer needs and cannot currently get.

---

## 11. What we do not build

**Durable execution is solved.** Journal, replay, resume-at-step-N, timers, cancellation —
that is Temporal's exact model, and Argo/Flyte/Dagster occupy the adjacent space. Building
it ourselves buys nothing.

**What nobody has built is admissible authority over the plan** — the static bound, the
three-way intersection, approval by shape, widening semantics on the definition. That is
ours.

Same posture as the git gateway: buy the substrate, own the entitlement translation.
Practically, this means the `Workflow` object should be expressible as a compilation
target — the definition is authored in our schema, and a backend adapter emits whatever
the chosen engine wants. Do not let the engine's DSL become the authoring surface, or the
authority bound becomes uncomputable for exactly the reasons in §5.

---

## 12. Worked examples

| Workflow | Nodes | Cycles | Authority bound | Planner |
|---|---|---|---|---|
| `coding-loop` | architect → developer ⇄ checker → publish | dev↔checker, max 3 | `github:read/write`, `jira:read`, `registry:read`, `ci:trigger` | yes |
| `package-tree-review` | collector → analyzer → reporter | none | `registry:read`, `jira:write` | no |
| `cve-triage` | normalizer → triager → justifier | none | `aikido:read`, `jira:write`, `slack:write` | no |
| `marketing-pipeline` | brief → draft ⇄ review → publish | draft↔review, max 2 | `drive:read/write`, `slack:write` | no |

The point of the last row: `marketing-pipeline`'s bound contains **no** `github`, no
`registry`, no `ci`. That is a static fact about the shape, checkable by an approver who
does not read the implementations — and it holds no matter which member role runs it.

---

## 13. Impacts on existing specs

1. **Task-execution diagram** — gains an object upstream of the planner. The planner no
   longer emits into a vacuum; it emits a walk through a named workflow revision.
2. **Admission** — becomes two checks (in-the-language, then within-authority) and a
   three-way intersection. The largest change in this document.
3. **`AgentRuntime`** — gains a list of workflows the runtime may execute. This is a
   fourth attenuation surface and should be authored, not implied.
4. **Data plane** — `io.handleType` makes the handle contract declared rather than
   conventional; the substrate binding (task repo vs blob CAS) becomes derivable per edge.
5. **Admin UI** — a workflow catalog view showing each definition's authority bound, and
   the revision-diff view from §10. The blast-radius component already exists.
6. **AI-workflow charter fit** — Wave 0 / Wave 1 envelopes now intersect with per-workflow
   bounds, which is strictly tighter than either alone.
7. **Upstream** — the static-bound argument is directly relevant to #2025 and #2109.

---

## 14. Open decisions

| # | Decision | Position | Cost of deferring |
|---|---|---|---|
| W1 | Two objects (`Workflow` + `Task`) vs one | two — grammar and sentence | **high** — the whole document |
| W2 | Declarative, non-Turing-complete definition | yes; the reachability test in §5 is the gate on every feature | **high** — irreversible once a program |
| W3 | Three-envelope intersection | intersect, never union | **high** — admission logic |
| W4 | Role catalog as a separate object | yes — separates shape from implementation choice | medium |
| W5 | Buy durable execution (Temporal-shaped) or build | buy; own the admission layer; our schema stays the authoring surface | medium — expensive either way, but building is worse |
| W6 | Promotion-from-run tooling | build early; it is how shapes are actually discovered | low — nice-to-have that becomes essential |
| W7 | Does `AgentRuntime` pin allowed workflows | yes, explicitly authored | medium |
| W8 | Conditional edges over a closed outcome set | permit; expressions do not | low, but the slippery one |
| W9 | Naming: `Workflow` / `Task` | adopt — avoids the `SandboxTemplate` / `ServiceTemplate` collision "template" would drag in | low |

---

## 15. Invariants added

1. **A task is a walk through a workflow revision, pinned by digest.** No un-typed tasks.
2. **Every cycle declares a static maximum.** Unbounded iteration is not expressible.
3. **A workflow's authority bound is computed, never authored** — it is the union over
   reachable nodes, and it is what approval-by-shape approves.
4. **Effective authority is the intersection of member role envelope, workflow bound, and
   admitted walk.** No component widens another.
5. **A plan outside the language is rejected, not escalated.** Malformed is not a request
   for authority.
6. **Raising a workflow's authority bound is a widening change** and goes through the same
   door as any other.
7. **Workflows reference step roles; the role catalog resolves implementations by pinned
   digest.** Shape and implementation are separately governed.
