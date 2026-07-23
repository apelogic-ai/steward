# Steward Data Plane — Task State, Storage, and the Tier 1 Block Inventory

Status: draft — discussion record converted to positions
Audience: ApeLogic engineering
Companions: *Solution Overview*, *Dev & Integration Spec*, *OpenShell Upstream Strategy*
Scope: where task state lives between steps, what earns a Tier 1 block, and what does not

> **Framing.** The control plane question ("who may do what") is settled in the
> companion specs. This document covers the **data plane**: what the agent reads and
> writes, where it persists, and who resolves authorization on it. The two are coupled
> in exactly one place — storage that is shared without per-user entitlement is a
> policy hole no envelope can close.

---

## 1. The rule

**State that outlives the sandbox does not live in the sandbox.**

This is the journal's rule from the task-execution design, applied to the data plane.
The journal already establishes that the record of *what happened* is an artifact
retained outside the runtime. Everything below is the same argument for *what was
produced*.

The corollary that does most of the work: **anything two sandboxes both touch is an
inter-agent channel, and every other channel in this system crosses a policy gate.**
A shared mutable volume, a shared KV namespace, a shared scratch directory — each is a
side channel that carries no attribution and no decision. That is the single test that
disqualifies most of the obvious designs.

---

## 2. Step handoff: the SHA discipline

### 2.1 Mechanism

Each step's journal entry carries an **input SHA** and an **output SHA**.

```
step N   fetch <input-sha> → work → commit → push refs/steward/task/<id>/step/N
                                             └─ output SHA written to journal
step N+1 fetch <output-sha of N> → work → …
```

- Refs, not branches. `refs/steward/task/<id>/*` stays out of the repo's branch list,
  is GC-able on terminal state, and does not trigger branch-protection evaluation.
- A task-scoped PVC may exist as a **warm clone**. If it is present and `HEAD` matches
  the expected input SHA, reuse it; otherwise clone fresh.
- **Correctness never depends on the volume.** RWO scheduling constraints and node
  affinity therefore degrade to a latency problem, not a failure mode.

### 2.2 Why not a shared task PVC as the primary

| | Shared PVC | SHA in journal |
|---|---|---|
| Inter-step handoff | unattributed file write | reviewable diff pinned to a content hash |
| Replay at step N | resumes into whatever the volume contains, possibly post-crash garbage | deterministic — resumes from a named commit |
| Parallel steps | requires RWX → NFS/CephFS/EFS as a load-bearing dependency | fan out from one SHA, fan in by rebase |
| Teardown | leaks unless tracked | ref + volume ref both in `status.refs` |

### 2.3 Consequences to accept

1. **Tier 0 gains an allowlisted destination** — the internal git gateway, not
   `github.com`. Bulk clone/push through `mcp-gw` as an MCP tool call is the wrong
   shape; this is a protocol endpoint, not a tool.
2. **The PVC reference goes in `status.refs`.** Our G6 teardown-completeness argument
   applies verbatim: teardown must be a traversal, not a sweep.
3. **Clone cost per step** — shallow + partial clone, and this is where
   `SandboxWarmPool` (§8 of the upstream strategy) earns its place.

### 2.4 What is *not* implied

The SHA discipline is **independent of sandbox granularity**. It is worth having whether
one sandbox spans the whole task or one spans each step.

---

## 3. Sandbox granularity is a tuning knob

One sandbox per task is a legitimate design. The deciding property is **whether the run
can park**.

The task-execution design claims *"paused run = zero compute; approval waits cost
nothing."* A task-scoped sandbox forfeits that: it either idles holding a pod through an
approval wait, or it dies and state is reconstructed anyway. Node eviction on a
multi-hour run has the same shape.

**Per-step (or per-segment) earns its cost when:**

- there is an approval gate mid-plan — parking is the point of a durable executor
- roles are real trust boundaries — a checker sharing process space with the developer
  is the same blast radius wearing a different prompt
- toolchains are heterogeneous — otherwise one fat image must satisfy every role
- the horizon is hours-to-days, where eviction is a *when*

**One sandbox per task is fine when** the task is bounded in hours, has no mid-run human
gate, and the roles need no distinct trust boundary.

**Position:** the useful default is **per-segment** — the sandbox spans a contiguous run
of steps and dies at any parking point. The SHA discipline is what makes granularity a
tuning knob settable per workload rather than a design-time weld.

---

## 4. The git gateway (Tier 1)

### 4.1 Two roles, kept separate

| Role | Shape | Keying |
|---|---|---|
| **Pull-through cache** of upstream repos | lazy, not a scheduled full mirror — cache-miss is slow, never broken; no "which repos" config to maintain | `(upstream-org, repo)` |
| **Scratch ref store** for `refs/steward/task/<id>/*` | high-churn, GC-able, per-task namespace | task id |

Point the scratch store's `objects/info/alternates` at the cache's object store: pushes
become delta-cheap and clones near-free.

### 4.2 Storage scope vs. access scope

**Org-scoped storage, per-user entitlement.** Conflating the two is the failure mode.

- Per-user storage is nonsense — N copies of identical objects, N sync paths.
- Per-org storage **with org-wide read** is a security regression: it flattens GitHub's
  repo-level ACLs. A private repo an engineer cannot see upstream becomes clonable
  through the mirror. That is a lateral-movement primitive.

So: shared storage, entitlement resolved per acting user. The sandbox presents its HOP-1
JWT; the gateway resolves entitlement by email against upstream — exactly as `mcp-gw`
resolves provider tokens. Verify at admission (bind the task to a repo set) and
**re-verify on a short TTL during the run**. Standing delegation with stale entitlement
is precisely the #1757 shape already documented upstream.

### 4.3 Push path and attribution

Do not push to GitHub with the user's token — it expires mid-run with nobody present to
re-authenticate.

- The gateway pushes with an **App installation credential**.
- Commit author is set to the user's email — that is metadata, not credential.
- The acting agent is recorded in the commit trailer and the PR body.
- **Signing is where we stay honest:** a bot-signed commit must not claim to be
  user-signed.

### 4.4 The underrated win

One mirror means **one GitHub API budget**. Fanning twenty step-sandboxes at
`github.com` directly meets secondary rate limits well before it meets a policy problem.

### 4.5 Build posture and off-ramp

**Buy, don't build.** Gitea or `git-http-backend` behind an auth proxy. The smart-HTTP
surface is two endpoints; the work is entitlement translation, not git.

**Off-ramp:** for a single-org deployment at modest scale, skip it. Allowlist
`github.com` at Tier 0 with a supervisor-injected fine-grained token, and use a task PVC
for scratch. The gateway earns its cost at clone volume, at rate limits, and at the
compliance line where *"no sandbox egresses to SaaS"* must be true on the wire.

**Constraint if deferred:** do not let the remote URL become load-bearing in the step
contract, or swapping it later touches every role image.

---

## 5. Why not GitHub as the persistence layer between steps

Commit/push/pull between steps **is** the design. The question is only where the remote
points. Pointing it at `github.com` costs:

- **Every intermediate push is an event.** Push triggers workflows, webhooks,
  notifications, branch-protection evaluation. Forty step commits is forty CI runs
  unless filtered — and the filter works only because non-branch refs don't trigger most
  of that, which makes a GitHub behavior detail load-bearing for cost control.
- **The ref-namespace constraint is unenforceable.** "May push only to `refs/steward/*`"
  is a git-layer rule; Tier 0 sees an opaque TLS session, not a refspec. A token that can
  push scratch can push branches and force-push. On an internal mirror the constraint is
  enforced **server-side by something we control** — most of why the mirror exists.
- **Retention we don't own.** Dangling commits remain reachable by SHA long after the ref
  is deleted. Intermediate agent state is the material most likely to contain a
  half-written secret, and the material we least want durable in a SaaS.
- **Availability coupling.** GitHub degrades → every in-flight step stalls at the
  handoff. With local scratch, only fetch-upstream and final publish block.

**The hinge:** inter-step handoff and publication are two different jobs. Handoff is
ephemeral, high-churn, private to the run. Publication is durable, reviewed, and the
deliverable. GitHub is very good at the second and structurally wrong for the first —
paying publication semantics for state whose defining property is that it gets thrown
away.

**Position: scratch local, publish once.** The final PR is a squash from the last step's
SHA onto a real branch, pushed by the gateway. It is the only thing GitHub ever sees,
which also makes CI clean: one push, one run, on reviewable code.

### 5.1 Why not GitHub as the governance layer either

Separately raised and separately rejected. GitHub can express *which repos* and nothing
else in the envelope — no budget, no model set, no tool action classes, no TTL. One axis
out of five, and the one axis we already decided not to model ourselves.

- **Containment** — Actions runners have no default-deny egress. Building it means
  building Steward inside a runner with worse ergonomics.
- **Authority** — environment protection rules do park without burning a runner (credit
  where due), but approving there makes GitHub the state authority, the exact split we
  rejected for Jira. And the approval carries no structured delta: you approve a job, not
  `budget.amount 300 vs ceiling 250`.
- **Delegation** — an App installation token is a bot identity. It cannot express
  "acting as Leo, attenuated to a subset of Leo's reach." That is the mint, and it is
  what #1756/#1757 are about upstream.
- **Spend** — accrues on the inference path, invisible to GitHub. G4.
- **Resume granularity** — re-running a job restarts the job; the journal resumes at
  step N.
- **Structural** — coding is a *workload* on Steward, not its purpose. Binding teams to
  GitHub teams leaves every non-code agent without a home.

**What GitHub should own:** repo entitlement (already settled — the mirror resolves per
user upstream), code review as the output gate, and **team rosters read from GitHub
teams rather than authored twice**. Roster in GitHub, authority in Steward.

---

## 6. Teams

**Team = workspace.** RFC-0011 supplies the scoping substrate; our envelope layers
inside it. This closes Overview §8.5 without inventing a third boundary. Constraints:
19-char names, immutable after creation, and Phase 2 is where membership becomes
authorization data rather than a name anyone can guess.

**The sharp edge: attenuation is per-person, ownership is a set.** A task owned by a team
still has exactly one acting user, and the agent gets a subset of *that person's* access.

> **Invariant.** Team membership decides **who may be** the acting user. It never widens
> what that user can do.

If team-owned ever resolves to the union of members' access, that is a
privilege-escalation primitive that reads as a convenience feature — the
highest-privileged member's reach becomes ambient to everyone who can file a task. The
same trap one level up: a person in two teams must not receive the union of two
envelopes. **Team is selected per request, explicitly, and admission runs against that
one envelope.**

**Free consequences:**

- The ref namespace is already task-scoped, not user-scoped. No change.
- The mirror inherits GitHub's team ACLs, because entitlement resolves per acting user
  against upstream. Teams are not modelled in the gateway at all.
- Journal + parked steps *is* the handoff primitive: one person's run parks at an
  approval, a teammate clears it, the run resumes. But **approval authority is not
  delegation authority** — approving a step does not re-attribute who the agent acts for.

**Open, decide now:** reassignment mid-task. A handoff to a new acting user is a **new
delegation** and must re-enter admission against the new person's envelope — not a silent
owner-field update. The journal stays; the identity binding is re-minted. Cheap now,
painful once someone writes the owner as a mutable field.

---

## 7. The Tier 1 membership test

A block belongs in Tier 1 if it does **all three**:

1. **Owns state no single sandbox can see** — a catalog, an aggregate, a shared corpus.
2. **Custodies a credential no sandbox may hold.**
3. **Resolves an authorization decision per acting user.**

Fail (3) and it is an allowlisted destination, not a gateway. Fail (1) and it is
per-agent config. The test is about **who resolves authorization**, not about importance
or size.

---

## 8. Block inventory

| Block | State | Storage scope | Authorization principal | Verdict |
|---|---|---|---|---|
| `mcp-gw` — tools | shipped | org | acting user (email → provider tokens) | Tier 1 |
| LiteLLM — inference | shipped | org | per-agent virtual key; org spend | Tier 1 |
| **Git gateway** — cache + scratch refs | proposed | org | acting user, resolved upstream | Tier 1 |
| **Package registry proxy** — npm/PyPI/crates/Go | proposed, **urgent** | org | mostly uniform; supply-chain policy point | Tier 1 |
| **Fetch proxy** — docs/web read | proposed | none (transit) | per-domain policy | Tier 1 |
| **Notification egress** — Slack/email | proposed | none (transit) | acting user; attribution rewrite | Tier 1 |
| **Knowledge layer** — project + cross-project semantics | proposed, hardest | org | provenance-label lattice | Tier 1 |
| Blob CAS — large artifacts | proposed | task-scoped GC | digest-only reachability | **Tier 0.5** |
| Mutable KV between steps | — | — | none | **Rejected** |
| Artifacts with an existing SoR (Doc, ticket, sheet, CRM) | — | — | already `mcp-gw` tool policy | **No new block** |

**Sequencing: registry proxy first.** Default-deny Tier 0 means agents cannot
`npm install` at all today — it is blocking, concrete, and reuses the mirror pattern
wholesale. Likely the same box as the git gateway.

Notes on the two transit blocks:

- **Fetch proxy** — inbound is the prompt-injection surface, outbound is exfil-shaped.
  Whether it is an `mcp-gw` tool or its own block depends on whether per-domain policy is
  wanted, which it will be.
- **Notification egress** — an agent that can message people must have its messages
  attributed as *Leo's agent*, never as Leo. Cheap early, ugly to retrofit.

---

## 9. Scratchpad: blobs yes, KV no

**Blobs pass the storage test and fail the authorization test.** Content-addressed,
immutable, digest recorded in the journal. Authorization is then near-vacuous: you can
only fetch what you hold the digest for, and digests only come from the journal. That is
an allowlisted destination with task-scoped GC — **Tier 0.5**. Build it; skip the
gateway ceremony.

**Mutable KV is rejected.** Shared mutable state between step sandboxes is the
unattributed side channel wearing a nicer API. If step 7 must tell step 9 something, that
is a journal entry — attributable, replay-safe, inspectable. A KV store is the journal
with the accountability filed off. The only admissible KV is single-writer-per-task
ephemeral scratch, which is just the PVC.

---

## 10. Non-code tasks

Non-code tasks are the majority of the platform, and "the repo is the handoff" quietly
assumed a repo. The tier verdicts do not change — tier is about who resolves
authorization — but the storage answer needs stating.

**First fork: does the artifact already have a system of record?** A doc, a ticket, a
spreadsheet, a CRM record → that is an `mcp-gw` tool call against Drive or Jira, governed
by existing tool policy. **No new block.** Steward must not accumulate a shadow document
store beside the one the org already runs. Only genuinely intermediate state — thrown
away at task termination — needs a home here.

**Second: intermediate state is already solved.** An **ephemeral task repo with no
upstream**, in the scratch ref store. Markdown, JSON, extracted text, structured
findings. This yields versioning, diffs between steps, atomic multi-file commits, and the
same GC and `status.refs` teardown story — for zero new infrastructure and zero new
role-image plumbing. **The step contract is identical whether the task is code or not**,
which is worth a great deal on its own.

**The dividing line is text-ish vs blob, not code vs non-code.** PDFs in, images, media,
datasets, model outputs → blob CAS, digest in the journal, task-scoped GC. Chunked CAS if
delta upload on a large evolving artifact is ever forced; not before.

**Unchanged for non-code work:** mutable KV is still the trap, and the provenance-labeling
problem is identical — a research artifact synthesized from sources Maya cannot see is
the same declassification hazard as a code summary.

**Watch:** non-code tasks tend to produce *one* deliverable that wants to land somewhere
durable — a Doc, a ticket, an email. Same publish-once shape as the PR, and the same
place the attribution question bites. The output lands as **Leo's agent's** work, not
Leo's.

---

## 11. The knowledge layer

Real Tier 1, and the hardest block in the system — because it is a **declassification
engine**.

**The failure:** an agent acting as Leo reads a repo Maya cannot see, summarizes it into
project memory, and Maya's agent reads the summary. An ACL has been laundered through a
summarization step. This is worse than the org-mirror version of the same problem,
because a summary cannot be diffed against its source to prove what leaked.

**The invariant:**

> Knowledge carries the provenance label of every source that produced it, and read
> requires entitlement to **all** of them.

**Labeling happens at write time** — the provenance set is recorded with the item — never
as a read-time filter, because the summary has already destroyed the evidence a filter
would key on.

**Cross-project semantics collapse to a lattice problem.** An item spanning two projects
unions their label sets, so its audience narrows to people entitled to both. That is
correct behaviour, and also why cross-project knowledge is largely useless until somebody
declassifies it.

**Therefore declassification is an approval action** — human, evidenced, structured delta,
routed to the approval queue that already exists. Not a background job, not a heuristic.

---

## 12. Open decisions

| # | Decision | Position | Cost of deferring |
|---|---|---|---|
| D1 | Sandbox granularity default | per-segment; parking points are the boundary | low — SHA discipline keeps it tunable |
| D2 | Git gateway now, or Tier 0 → `github.com` with PVC scratch | defer is acceptable at single-org scale | low, **if** the remote URL stays out of the step contract |
| D3 | Registry proxy | build first; same box as the git gateway | **high** — agents cannot install packages today |
| D4 | Mid-task reassignment semantics | new delegation, re-enters admission | **high** — retrofit is painful once owner is mutable |
| D5 | Fetch proxy as `mcp-gw` tool vs. own block | own block if per-domain policy is wanted | medium |
| D6 | Knowledge-layer labeling granularity (item / chunk / field) | undecided | **high** — schema-level, hard to change later |
| D7 | Do test/build artifacts and dependency caches ride the PVC or the blob CAS | undecided; they are the actual reason to keep the PVC | medium |
| D8 | Team rosters synced from GitHub teams | yes — roster in GitHub, authority in Steward | low |

---

## 13. Invariants added by this document

1. **State that outlives the sandbox does not live in the sandbox.**
2. **Inter-step handoff is a content hash recorded in the journal**, never a shared
   mutable volume. A volume is a cache; correctness never depends on it.
3. **Storage scope and access scope are separate decisions.** Shared storage with
   shared read is a lateral-movement primitive; entitlement resolves per acting user.
4. **Tier is decided by who resolves authorization**, not by importance or size.
5. **Knowledge items carry the provenance label of every source; read requires
   entitlement to all.** Declassification is an approval action.
6. **Attenuation is per-person.** Team membership decides who may be the acting user; it
   never widens what that user can do.
7. **Scratch is local and disposable; publication is a single reviewed push.** Do not pay
   publication semantics for ephemeral state.
8. **Agent output is attributed to the acting user's *agent*, never to the user.**
