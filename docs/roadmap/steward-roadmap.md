# Steward Roadmap — Zero to PoC v0.1.0

Status: draft — stack decisions, slice plan, and upstream dependency schedule
Audience: ApeLogic engineering
Companions: *Solution Overview*, *Dev & Integration Spec*, *Data Plane Spec*,
*Workflow and Task*, *AI-Workflow Charter Fit*, *OpenShell Upstream Strategy*

> **Thesis.** The specs have accreted into two systems. **Plane A** is the provisioning
> and governance control plane — manifest, envelope, admission, escalation, identity,
> spend, revocation. **Plane B** is workflow and task execution — `Workflow`,
> `WorkflowBinding`, `Task`, the journal, the executor, the data plane. B depends
> entirely on A. A ships alone and is the original product.
>
> **v0.1.0 is Plane A only.** Not because B is less interesting — it is more
> interesting — but because W1–W14 are mostly high-cost-to-defer *schema* decisions,
> and a schema nothing has run through has not been validated. Slicing B before A ships
> means designing the authority language before the authority spine is proven on the
> wire.
>
> Seven slices, S-1 through S5. Each ends in one falsifiable assertion. If it cannot be
> demonstrated, the slice is not done.

---

## 1. Definition of done for v0.1.0

One real agent, for one real user, in a real cluster, through both doors:

1. A member requests an agent through the API. It fits their role envelope. It
   provisions with no human in the loop.
2. It calls a GitHub tool as **that member**, through `mcp-gw`, holding none of their
   credentials.
3. It consumes inference through a per-runtime LiteLLM key, against a real budget.
4. The member requests a modification that exceeds the envelope. It parks. Jira is
   filed with the deltas. An admin approves in Steward with the Jira link as evidence.
   The grant binds to that runtime; the role ceiling does not move.
5. The agent is terminated. Its identity, its inference key, and its tool reach are all
   dead, verified by attempting each.

Anything not required by those five sentences is out.

---

## 2. Stack — locked

| Layer | Choice | Rationale |
|---|---|---|
| Language | **Rust**, tokio | Matches the OpenShell codebase we already read (router, prover); one language across mint, controller, API, and any upstream contribution |
| Operator | **kube-rs** `Controller` + `CustomResource` derive, schemars | Smaller ecosystem than kubebuilder — more reconcile scaffolding by hand, less generator support. Accepted |
| API | **axum**, REST/JSON, OpenAPI via `utoipa` | Same runtime as the controller; no second executor. Generated client for the UI and integrations |
| Desired state | **CRDs** | Locked in *Solution Overview* §8.1. k8s-native audit, RBAC, GitOps, admission-webhook gating |
| Operational state | **Postgres 16**, `sqlx` | §3 |
| Identity | **SPIRE** JWT-SVID → Steward mint → **HOP-1** JWT, **EdDSA (Ed25519)** + JWKS | The verifier pins no algorithm — it accepts what the issuer's JWKS advertises (`hop1.ts`, verified `ce683bf`). Ed25519 is therefore free, and the mint registers as an additional trusted issuer beside the existing one (D6) |
| Policy | **OPA**, Rego bundles authored by Steward | Locked. Not a second policy engine — projections into OpenShell and `mcp-gw` |
| Inference | **LiteLLM** — key custody, catalog, budget | Locked, and after §5.4 closed it is the *only* enforcement point for `spec.llms[]` |
| Tools | **`mcp-gw`** — catalog, per-user credential custody, OPA decision | Locked. No per-agent grant object exists; teardown makes no `mcp-gw` call |
| Escalation | **Jira** — channel, never authority | Locked |
| Packaging | Helm chart; Flux later | Matches the Sveltos/Flux distribution road |
| Telemetry | OTel, structured audit at each enforcement point | Known gap: R3 |

### 2.1 Crate layout

```
crates/
  steward-types/        CRD types; the source the CRD YAML is generated from
  steward-ports/        the interfaces (§2.5). Zero vendor dependencies
  steward-admission/    envelope check, delta computation — THE shared crate
  steward-store/        sqlx; the only module that writes Postgres
  steward-mint/         SVID → HOP-1; separate deployment, holds the signing key
  steward-controller/   kube-rs reconciler + validating admission webhook
  steward-apiserver/    axum; the portal and all integrations talk to this
adapters/
  openshell/            sandbox runtime (class B)
  litellm/              inference plane
  mcp-gw/               tool plane
  jira/                 DecisionChannel (deferred)
  spire/                workload attestation
  opa/                  policy distribution
  fake/                 in-memory implementation of every port — §2.5.3
```

Core crates depend on `steward-ports` and never below it. Adapters depend on ports plus
one vendor SDK each.

Three deployments: `apiserver`, `controller` (webhook co-resident — both are k8s-facing
and share the admission crate), and `mint`. The mint is separated because it is the one
component holding a signing key, and its blast radius should not be the API's.

### 2.2 Two front doors, one admission library

A real CRD means desired state has **two writers**: the REST API, and
kubectl/Flux writing the CRD directly. If the envelope check lives only in the API,
GitOps is a governance hole.

| Door | Behaviour on over-envelope | Why |
|---|---|---|
| **Validating webhook** | **hard deny**, with the counterexample in the rejection message | Non-bypassable. Cannot file Jira or park — an admission webhook is a synchronous yes/no |
| **REST API** | compute structured deltas → file Jira → park as `Pending` → notify | The ergonomic path. Does the nice things the webhook structurally cannot |

Both call `steward-admission`. The webhook enforces; the API enforces *and* escalates.

Two consequences worth stating now:

- **The webhook is validating only, never mutating.** Steward does not silently default
  or clamp a manifest. Missing required fields are rejected, not filled — this is W8's
  "no silent defaults" applied one level up.
- **`kubectl apply` is break-glass, not a workflow.** It works, it is governed, and it
  produces a worse experience on purpose.

### 2.3 API group — decide before slice 0

*Solution Overview* flags "Steward" as a working name pending trademark check. The CRD
API group is the single most expensive string to rename later: stored objects, RBAC
bindings, GitOps manifests, and every consumer's client.

> **Position: do not put the product name in the API group.** Use
> `agents.apelogic.ai/v1alpha1`. The brand rename then touches documentation and the
> binary names, and not one stored object.

### 2.4 Repository — one monorepo

**Decision: a single Cargo workspace, plus the portal, in one repository.**
Consistent with house convention (both `observer` and `semantic-grid` are
monorepos), but the decisive argument is narrower than convention:

> `steward-admission` is depended on by **both** the webhook and the API.
> Invariant 3 says there is no third door. Two repositories means a versioned
> crate between them and a window in which the two doors enforce different
> rules. That is a governance hole created by repository layout, and no amount
> of discipline closes it.

Everything else follows: `steward-types` generates the CRD YAML that every
consumer needs in lockstep; a slice touching schema + admission + migration +
API + portal types is one atomic change; and "one commit per slice" is not
achievable across repositories.

```
steward/
  Cargo.toml                  # workspace
  crates/                     # §2.1 — core; no vendor dependencies
  adapters/                   # §2.5 — one crate per vendor, plus `fake`
  policy/                     # Rego bundles + opa tests
  migrations/                 # sqlx; append-only
  manifests/                  # GENERATED CRD YAML — committed, CI-verified in sync
  charts/steward/
  conformance/                # G-1…G-6 (§8.1)
  xtask/                      # task runner; see below
  web/                        # portal; generated OpenAPI client
  third_party/openshell-patches/   # each patch beside its upstream attempt (§8.4)
  docs/                       # these specs
  AGENTS.md  CLAUDE.md
```

**Task runner: `cargo xtask`, not `just` or `make`.** A plain crate in the
workspace, aliased in `.cargo/config.toml` so `cargo xtask <task>` works with
nothing installed beyond the Rust toolchain that is already required. Three
reasons it beats `just` here specifically:

- **Nothing extra to install.** Contributors and agents get the full gate set
  from a clean checkout. `just` is one more prerequisite to discover the hard
  way.
- **These tasks need logic, not one-liners.** Standing up a kind cluster with a
  pinned OpenShell, running the conformance suite across two lanes, verifying
  generated CRD YAML against `steward-types` — that is a program. Written in
  Rust it can reuse `steward-types` directly rather than re-deriving the schema
  in shell.
- **CI and local invoke the same code.** Workflow files become
  `cargo xtask ci`. A local-vs-CI divergence becomes a bug in `xtask` with a
  test, instead of a shell snippet that drifted.

The cost is honest: an `xtask` task is more verbose to write than a `just`
recipe. For one-line wrappers that is a real loss, and for everything above it
is not.

**One deployment artifact, not two.** Steward installs from its helm chart in every
environment — local kind, shared DEV, and production. Terraform and Ansible provision the
*host*; they do not describe the application twice. A DEV that is described differently
from production is a DEV that does not test the install path, which is the path customers
exercise (D12, D17).

**Deployment boundaries are not repository boundaries.** The mint ships as its
own deployment with its own RBAC and blast radius, and lives in the workspace
like everything else — with its own nested `AGENTS.md`, because it holds the
signing key.

Three things stay **out**:

| Out | Why |
|---|---|
| **Existing internal systems** | They have their own lifecycles and are ordinary API clients (§2.6.1). Any migration adapter lives in their repository, not ours |
| **An OpenShell fork** | We carry patches, not a fork. `third_party/openshell-patches/`, each with its upstream attempt and exit condition |
| **Customer/tenant config** | Not a v0.1.0 concern; when it arrives it is data, not code |

One inconsistency to settle now: `observer` uses `master`. Steward uses `main`,
and `AGENTS.md` is written against `main`. Worth normalising across the org, but
not on this roadmap's budget.

### 2.5 Dependency classes — what we are allowed to depend on

> **If a plane has more than one plausible vendor, Steward depends on an interface and
> never on the vendor.** The vendor is an adapter, chosen by configuration, and its name
> appears nowhere above the adapter boundary.
>
> Shipping exactly one adapter is fine. Shipping no interface is not.

| Class | Meaning | Members |
|---|---|---|
| **A — Substrate** | Commodity, open, everywhere. Replacing it means a different product. No adapter; the coupling is accepted deliberately | Kubernetes, Postgres |
| **B — Strategic single-vendor** | One vendor by choice, behind a seam, with no second implementation planned | OpenShell |
| **C — Vendor plane** | More than one plausible vendor. Interface mandatory | inference gateway, tool gateway, escalation channel, workload attestation, policy distribution |
| **D — Not a dependency** | Frequently mistaken for one | GitHub, Slack, individual model providers, individual tool providers |

**Class A.** Kubernetes is not a choice we are hedging — the CRD-and-controller model *is*
the position taken in *Solution Overview* §8.1. Postgres is commodity, and we use `jsonb`
and partitioning on purpose. Neither warrants an abstraction: both are open and
ubiquitous, and a portability layer over SQL is a well-known route to the worst of both.

**Class B — OpenShell, stated honestly.** OpenShell is a hard dependency **by strategy,
not by necessity.** Other sandbox runtimes exist. We picked one, we are investing upstream
in it, and a second sandbox adapter would halve that investment in both directions. So:
the seam exists (`adapters/openshell`, invariant 9), we deliberately do not build a second
implementation, and §8 manages the risk that follows. If the bet ever has to be unwound,
the seam bounds the blast radius to one crate — which is why it is worth having even with
a single implementation.

**Class C — the planes.**

| Plane | Port | PoC adapter | Why it cannot be hard |
|---|---|---|---|
| Inference gateway | `InferencePlane` | LiteLLM | Portkey, Kong AI Gateway, Cloudflare, Bedrock/Vertex. Enterprises have already standardised here and will not re-platform for us |
| Tool gateway | `ToolPlane` | `mcp-gw` | First-party is not the same as structural. A customer may already run an MCP gateway |
| Frontend (outbound) | `DecisionChannel`, `NotificationSink`, `SessionRelay` | Jira, the portal, Burble (Slack) | §2.6 — a family, not one port |
| Workload attestation | `WorkloadIdentity` | SPIRE | The interface is the SPIFFE Workload API, an open spec. The adapter is nearly free |
| Policy distribution | `PolicySink` | OPA bundle push | See below |

Policy carries a nuance: OPA is **partly imposed rather than chosen**, because OpenShell
and `mcp-gw` are the consumers and they speak OPA. So abstract the act of publishing a
decision surface; do **not** abstract the bundle format. Authoring and composition are
ours and portable. Distribution speaks whatever the consumer speaks.

**Class D.** **GitHub is not a dependency of Steward.** It is our code host and CI — a
development dependency — and a tool-catalog entry the agent reaches through the tool
plane. Neither is architectural. The git *gateway* in the data plane (post-v0.1.0) *is* a
class C plane, `GitHostingPlane`, with GitLab, Gitea, and Bitbucket as plausible
alternatives. Chat surfaces belong to a connector, never to Steward (invariant 8). Individual model providers sit behind the
inference gateway and individual tool providers behind the tool gateway; Steward names
neither.

#### 2.5.1 A port is defined by what Steward needs, not by what the vendor offers

The failure mode is an abstraction derived from one implementation — a LiteLLM-shaped
trait with one implementor, which is LiteLLM plus indirection and no portability at all.
Derive each port from the **guarantee it serves**:

- `InferencePlane` exists because G-5 and the budget-exhaustion transition need
  enforcement. That is what it must express.
- `DecisionChannel` exists because Steward is state authority and needs an audit-visible
  external record of a human's decision. That is all it must express — see §2.6 for why
  it must be structurally incapable of expressing more.

#### 2.5.2 Adapters declare capabilities; Steward refuses rather than degrades

Not every gateway enforces spend. Not every tracker carries structured fields. An adapter
**declares what it provides**, and Steward refuses to admit a runtime whose guarantees the
configured adapter cannot deliver. It does not silently downgrade.

This is the unpriced-model lesson generalised: a budget that looks enforced and is not is
worse than no budget at all. Adapter capabilities feed the guarantee register — configure
an inference gateway that cannot enforce an allowlist and **G-5 moves off `provided`,
visibly**, by the same derivation that governs everything else in §8.1.

#### 2.5.3 The second implementation is a fake

An interface with one implementor has never been tested for vendor-shape. Building two
real adapters for a PoC is waste. So `adapters/fake` implements **every** port in memory,
and the port conformance suite runs against both.

The fake is the second implementation. It costs little, it proves the port is not
vendor-shaped, and most of the test ladder stops needing a live LiteLLM to run.

#### 2.5.4 Ports evolve; they are not versioned

All eight ports are defined in S-1, including those with no real adapter in v0.1.0. The
risk that creates is a port that sits for months with only `fake` behind it, is wrong by
the time a real adapter arrives, and has been designed around in the meantime as though
settled. Two answers, and neither is a version number.

**Traits are not versioned.** They are in-tree, in one workspace, compiled in lockstep.
Change a trait and the compiler enumerates every implementor. A `V1`/`V2` pair buys
nothing here except two things to maintain.

**Wire contracts are versioned**, because they cross a repository or process boundary:

| Contract | Crosses to | Mechanism |
|---|---|---|
| CRD schema | kubectl, GitOps | k8s API groups — `v1alpha1`, conversion webhooks. Already solved |
| REST API | portal, connectors, integrations | explicit version in the path |
| **HOP-1 claim set** | `mcp-gw`, git gateway, notification | **explicit version field** — it has already been re-cut once |
| **`SessionRelay` events** | out-of-tree connectors | **explicit version** — a connector upgrades on its own schedule |

The other six ports have in-tree adapters only, and speak vendors' wire protocols, which
are the vendors' to version.

**Evolution techniques, in place of versions:**

1. `#[non_exhaustive]` on the event enum and every struct crossing a port. Adding a
   variant or a field stops being a breaking change.
2. Capability declaration rather than trait growth (§2.5.2) — a vendor that cannot do
   something declares that, and the trait does not grow a method per quirk.
3. New methods carry an `Unsupported` default, so adding one does not touch six adapters.
4. Sealed traits where a port should not become a public API by accident.

**Maturity is declared and checked**, the same shape as the guarantee register:

- **`provisional`** — no non-`fake` implementor. Expect change; do not build against it as
  though stable.
- **`proven`** — at least one real adapter implements it and the port survived contact.

*`cargo xtask ports --check`* asserts both directions: nothing marked `proven` without a
real implementor, nothing with a real implementor left `provisional`. A confidence level a
human types is one that drifts.

In v0.1.0 five ports reach `proven` — `InferencePlane`, `ToolPlane`, `DecisionChannel`,
`WorkloadIdentity`, `PolicySink` — and three stay `provisional`: `NotificationSink`,
`SessionRelay`, `GitHostingPlane`.

#### 2.5.5 Enforced, not remembered

*`cargo deny`.* Only `adapters/<vendor>` may depend on that vendor's SDK. No core crate
may. Same mechanism that already holds the layering rule.

A vendor name may appear in exactly two places: an adapter crate name, and configuration.
Never in a core type, never in the CRD schema, never in `steward-admission`.

**Cost, honestly.** A trait boundary and an extra crate per plane. For Jira and SPIRE it
is nearly free. For the inference plane it is real design work, because LiteLLM currently
does three jobs — key custody, catalog, spend enforcement — and the port must name all
three without assuming one system performs them all. That is the plane worth spending the
time on, and it is precisely the one where re-platforming later would cost the most.

### 2.6 The frontend plane — outbound ports, inbound API

Jira and GitHub are not only escalation sinks. A ticket can *start* work, a PR comment can
*request* it, and both can *receive* the result. A chat connector is the same shape. So the
question is whether there is one first-class frontend plane.

**Answer: the plane is real, it is outbound only, and it is a family of narrow ports
rather than one wide one.**

#### 2.6.1 Inbound needs no port — it is the API

A Jira ticket that starts work, a GitHub Action, a chat connector, and the portal are all
doing the same thing: submitting a well-formed request. That is what §2.2's REST API is for.

> **Inbound is always push-to-API.** A channel-specific translator — Jira webhook
> receiver, GitHub App, chat connector — lives on the channel's side and calls Steward as
> an ordinary client. Steward never polls a ticketing system and never grows an inbound
> adapter.

Two things this buys. §2.2's "two front doors, no third" survives adding a fifth input
channel, because a channel is not a door. And adding an input channel touches no core
code — which is the actual test of whether the boundary is in the right place.

The translator authenticates as itself and acts on behalf of a resolved human. That is a
service principal acting for a user, which is precisely the `Principal` shape §4 reserves
and does not implement in v0.1.0. The reservation was worth making.

#### 2.6.2 Outbound is three ports, not one

| Port | Direction | Reply | v0.1.0 |
|---|---|---|---|
| `DecisionChannel` | Steward → human | **a decision comes back** | Jira, portal |
| `NotificationSink` | Steward → human | none | deferred |
| `SessionRelay` | Steward → human, streaming | approval round-trip only | **port + events defined**, unimplemented (§2.6.3) |

One wide `FrontendPlane` would force every adapter to stub most of it. Three narrow traits
compose: an adapter implements what it supports and declares the rest absent, which is the
capability mechanism §2.5.2 already defines.

#### 2.6.3 `SessionRelay` — streaming is a requirement, not an enhancement

Chat connectors such as Burble, and any terminal-like surface, are **streaming
consumers**. If the port
family is designed around request/response and streaming is bolted on later, the event
contract gets cut twice — and re-cutting a contract after consumers key off it is a cost
this project has already paid once, on the identity claim set. Do not pay it again.

So `SessionRelay` and its event enum are defined **now**, in v0.1.0, alongside the other
two. Implemented: none of it.

**The relay coalesces; the adapter declares granularity.** The producer side is one typed
event stream (§7.1). The consumer side is not uniform — Slack has no token-streaming
primitive and rate-limits message edits to roughly one per second, while a TUI or an SSE
web client can take every token. If each adapter solves that itself, every adapter writes
its own debouncer and each gets it subtly wrong.

| Granularity | Adapter declares it can take | Fits |
|---|---|---|
| `token` | sub-turn deltas as they arrive | TUI, web/SSE |
| `coalesced(interval)` | batched updates at a stated rate | Slack — post once, `chat.update` on the interval |
| `checkpoint` | only semantically complete units: `tool_result`, `turn_end`, `parked_for_approval` | email, a ticket comment |

Downsampling happens once, in the relay. Adding an adapter is declaring a granularity, not
writing a buffering loop.

**Backpressure never reaches the sandbox.** The agent is doing real work; the stream is
observation. A slow or disconnected subscriber is dropped, never allowed to stall the
producer — **and is told it was dropped.** A `Lagged(n)` signal is mandatory, not
optional: a viewer who silently missed `parked_for_approval` believes nothing is waiting
on them, which for this product is worse than showing them nothing at all.

**Every stream carries a monotonic sequence and ends explicitly.** Adapters resume from a
cursor after a reconnect or a thread scroll-back. Backfill beyond the relay's buffer comes
from the journal once Plane B exists; until then the relay's buffer is the limit and
adapters must handle "cannot backfill that far" rather than pretending. Every stream
terminates with an explicit `session_end` and a reason — without it, a Slack thread simply
stops and no one can tell whether the agent finished or the connection died.

**Entitlement is re-checked during the stream, not only at subscribe.** Sessions are
long-lived and access narrows mid-flight — the #1757 shape the *Data Plane Spec* already
commits to short-TTL re-verification for. A subscription whose entitlement lapses is
terminated, with a reason the adapter can render.

#### 2.6.4 The port must make the wrong thing unsayable

This is the risk that justifies the section. The rule already settled for Jira — Steward is
state authority, the external system is a channel — has to hold for every channel, and a
first-class frontend plane is exactly where it erodes, because **every new adapter is a
fresh opportunity to let a button be an approval.** A channel renders and carries; it never
decides.

So the type does the work, the way `Principal` does in §4:

> **A `DecisionChannel` adapter returns a `DecisionIntent`** — who, what, an evidence
> link, a timestamp — **never a `Decision`.** Steward evaluates the intent against the
> queue and records the decision. An adapter that could return a `Decision` would make
> "Steward is state authority" a convention rather than a fact.

Same move, one level down from where the Jira rule already sits. Jira never auto-approves
because Steward does not accept approvals from Jira; it accepts *evidence that a human
approved*, and decides.

#### 2.6.5 Each adapter is authoritative for exactly one fact

Every channel has its own principal namespace — Slack user ID, Jira account ID, GitHub
login. The email join key is load-bearing (R6), so:

> **A channel adapter resolves its principal to a corporate email, server-side, at bind
> time, never self-asserted — and is authoritative for nothing else.**

No exceptions, and no channel is authoritative for anything beyond that one fact.

**GitHub needs care here, and it is not a hypothetical.** Our upstream work happens on
public repositories. A GitHub comment treated as an input channel means anyone on the
internet can type the trigger phrase. The adapter must resolve to an **authenticated
organisation member with a verified corporate email mapping**, and reject everything
else — comment authorship is not identity. Getting this wrong is the R6 failure with a
public front door.

#### 2.6.6 Interactive vs. deferred is a capability, not a mode

"Sync and async" is a real distinction but the wrong axis — it describes transport. What
Steward needs to know is **whether a human is expected to be present**:

- **interactive** (portal, Slack) — a decision is expected in this sitting; short timeout,
  no reminder machinery
- **deferred** (Jira, GitHub) — a decision may take days; the request parks, reminders and
  expiry apply, and the parked state is the normal case rather than an error

An adapter declares which it is, and Steward's parking behaviour follows. Reuse §2.5.2's
capability machinery instead of inventing modes in the port.

#### 2.6.7 Where a connector fits

A chat connector is **a connector that contains an adapter, not an adapter.** Burble — our
Slack connector — is the working example, and the shape generalises to Teams or anything
else conversational.

- *Inbound:* it is an ordinary API client (§2.6.1).
- *Outbound:* it implements `NotificationSink` and `SessionRelay` for its surface. Steward
  calls it.
- *Above the plane:* clarification, elicitation, social context, narration. **No port
  expresses these and none should.** They are the reason a connector is a product rather
  than a config entry, and the reason the frontend plane stops where intent becomes
  well-formed.

That boundary is the same one everywhere: a connector owns intent and narration; Steward
owns admission through the journal.

#### 2.6.8 Scope for v0.1.0

Define all three ports **and the stream event enum** (§2.6.3). Implement `DecisionChannel`
only, with a Jira adapter and the portal. Inbound stays the API. Jira- and GitHub-initiated *tasks* are Plane B: they create
`Task` objects, which do not exist yet.

Cheap now, and the reservation is what stops the first non-Jira channel from being
retrofitted through the core.

---

## 3. Operational state — CRD + Postgres, single writer per fact

etcd is a coordination store. Append-only audit, a filter/sort/paginate approval queue,
and spend observation are none of the things it is good at. The split:

| Store | Holds | Authority |
|---|---|---|
| **CRD `spec`** | desired state | whoever authored it — portal, admin, GitOps |
| **CRD `status`** | *current* phase, refs, observed generation, spec digest | the controller, **single writer** |
| **Postgres** | history, admission decisions, queue detail, deltas, grants, spend observations | the controller, **append-only** |

No fact has two owners. Current phase lives in exactly one place. The *detail behind* a
phase — the deltas that made a runtime `Pending` — lives in Postgres. The controller
writes Postgres in a transaction, then reflects phase into status on reconcile. **Status
is a cache**; if it goes stale the next loop fixes it idempotently.

Postgres is not needed until slice 3. Slices 0–2 run CRD-only.

### 3.1 Tables (v0.1.0)

```sql
runtime_events        -- append-only, partitioned monthly
  runtime_uid, phase_from, phase_to, actor, reason, payload jsonb, at

admission_decisions
  runtime_uid, spec_digest, envelope_rev, verdict, deltas jsonb, at

approvals             -- the queue; this is the mockup's approvals screen
  id, runtime_uid, admission_decision_id, state, jira_key,
  decided_by, decided_at, rationale, evidence_url

grants                -- instance-bound exceptions. The anti-ratchet artifact
  id, runtime_uid, dimension, granted_value, approval_id, expires_at

envelopes             -- immutable revisions
  scope_kind, scope_ref, revision, spec jsonb, authored_by, at
  -- scope_kind is 'member_role' only in v0.1.0 (D5). Team and cost-centre
  -- scopes are admitted by the key and rejected by admission.
  -- Composition, when it arrives: intersect, never union.

spend_observations    -- projections FROM LiteLLM; not authoritative
  runtime_uid, window_start, window_end, observed_usd, source, at
```

**`grants` is the table that makes the anti-ratchet principle real.** An approved
over-limit request writes a row here bound to one `runtime_uid`. It never touches
`envelopes`. Saying yes to one agent is structurally incapable of raising the ceiling
for the role.

### 3.2 Two keying rules

1. **Join on `runtime_uid` (the Kubernetes UID), never on name.** Names get reused;
   OpenShell's own workspace name-reuse hazard is precisely this, and we filed it
   upstream. Do not repeat it in our own ledger.
2. **Spend is observed, not custodied.** LiteLLM is the source of truth and the
   enforcement point. `spend_observations` exists to drive the exhaustion transition and
   the fleet view. Steward is not a financial system of record.

---

## 4. Principal model — decided

Resolved ahead of its feature because `spec.actingUser` being assumed non-null
propagates into the CRD schema, the mint's signatures, every projection, and the teams
model. Retrofitting it later is a schema migration plus a mint rewrite.

```rust
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Principal {
    /// A standing, attenuated delegation of one person's access.
    User { acting_user: Email },
    /// Envelope authored directly by an admin, not derived from a person.
    /// Schema only in v0.1.0 — admission rejects it.
    Service { name: String },
}
```

- **`owner` is a distinct field from the acting user.** For a `User` principal they may
  coincide. For a `Service` principal the owner is a human, for accountability and
  notification, and is explicitly *not* the acting user.
- **The mint takes a `Principal`, not an email.** The acting-as claim carries either a
  delegated user or a service-principal id.
- **Only the `User` arm is implemented.** The service branch of `mcp-gw` credential
  resolution — a custodied credential with an authored scope, not per-user token
  resolution — is `unimplemented!()` with the type in place.

The trap being avoided is shipping `acting_user: String` threaded through the mint and
the projections. Model the enum; wire the user arm; stub the service arm.

---

## 5. CRD schema — frozen through slice 5

```rust
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(group = "agents.apelogic.ai", version = "v1alpha1", kind = "AgentRuntime",
       namespaced, status = "AgentRuntimeStatus", shortname = "ar")]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeSpec {
    pub principal: Principal,
    pub owner: Email,
    pub agent_type: AgentType,
    pub llms: Vec<ModelRef>,        // enforced at the LiteLLM key, NOT at Tier 0
    pub tools: Vec<ToolGrant>,      // provider:resource:action
    pub budget: Budget,             // monthly ceiling, currency explicit
    pub ttl: Duration,              // standing-delegation cap

    /// Plane B. Reserved, rejected if set in v0.1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindings: Option<Vec<BindingRef>>,
}

#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeStatus {
    pub phase: Phase,
    pub observed_generation: i64,
    pub spec_digest: String,        // what was admitted against
    pub refs: Refs,                 // sandbox, workspace, litellmKey — no mcpGrant
    pub conditions: Vec<Condition>,
    pub spend: Option<SpendSummary>,// cache of the Postgres observation
}

pub enum Phase {
    Pending, Admitted, Provisioning, Running,
    Suspended, Terminating, Terminated, Failed,
}
```

`status.refs.mcpGrant` is absent by design: `mcp-gw` has no per-agent grant object; tool
authorization is a pure OPA decision on signed identity claims. Teardown makes no
`mcp-gw` call.

### 5.1 State machine

```
create ─▶ Pending ──admit──▶ Admitted ─▶ Provisioning ─▶ Running
             ▲                                              │
             └── over-envelope ──▶ Pending(escalated)        │
                          approve (evidence) ──▶ Admitted    │
                                                             │
Running ── budget exhausted ─────────────────▶ Suspended ────┘ resume
Running | Suspended ── terminate | TTL ──▶ Terminating ─▶ Terminated
any ──▶ Failed
```

Only two transitions are **runtime-driven** rather than authored: budget-exhausted →
`Suspended` (slice 2) and TTL → `Terminating` (slice 5). Everything else is a decision
someone made.

---

## 6. The slices

### S-1 — Bootstrap *(the repository does not exist yet)*

**Build.** The workspace §2.1 and §2.4 describe: `Cargo.toml`, crate skeletons,
`adapters/` with every port stubbed and `adapters/fake` implementing all of them,
`xtask` implementing every command in `AGENTS.md` §7, a verified `deny.toml`, the
`.gitignore`, and CI wiring both lanes. All eight ports defined, with maturity markers
(§2.5.4).

**Proves.** That the rules are executable rather than aspirational. `deny.toml`'s
`wrappers` mechanism either enforces the §2.5.5 layering rule or it does not, and this is
when we find out — not at S3 when a vendor type has already leaked upward.

**Exit.** `cargo xtask ci` is green on an empty repository, and a deliberately-planted
violation fails it: add a vendor dependency to a core crate and watch `cargo deny` reject
it, then remove it.

**Size: S.** No external infrastructure, no open decisions outstanding, and every input is
already written down. It is also the one slice suitable for a long unattended run — it
touches none of the human-gated rules until the final push.

---

### S0.0 — Spike: the OpenShell gRPC surface *(prerequisite, not a slice)*

We are building on an API whose maintainers have not declared it a supported integration
contract — that is literally what #1613 is asking for. Before slice 0 has a schedule,
establish by reading and running:

- Which RPCs we depend on, and their stability in practice across the near-daily
  release cadence.
- The #2243 upgrade path: `{workspace}--{sandbox}--{service}` hostnames, **19-character
  name cap**, required `sandbox-name` / `sandbox-workspace` labels, drain-before-upgrade,
  re-create after migration 006.
- Confirm the 19-char cap and immutability apply to **workspace** names as we believe, and
  that teardown ordering works: workspace deletion is rejected while sandboxes exist, so a
  team's workspace outlives individual runtimes and our teardown deletes sandboxes only.

**Output:** a thin pinned client crate, an integration test against a pinned OpenShell
release, a naming scheme that fits 19 characters, **the guarantee register (§8.1), the
conformance suite that executes it, and `cargo xtask register`** — the generator, not
just the table. The suite exists before the walking skeleton,
because a walking skeleton built on a mis-inferred guarantee walks fine.

This is also our Lane H opening upstream — nobody in #1613 or #1719 is speaking as an
integrator with a shipping control plane, and after this spike we are.

---

### S0 — Walking skeleton
**Build.** `AgentRuntime` CRD + controller. Reconcile provisions an OpenShell sandbox;
delete tears it down. No policy, no identity, no budget, no admission.

**Proves.** The operator loop and the OpenShell gRPC surface work as an integration
contract.

**Exit.** `kubectl apply` a manifest → sandbox running, `status.refs` populated.
`kubectl delete` → sandbox gone, no orphans. Kill the controller mid-provision and
restart: reconcile is idempotent and converges. Size: **M**.

---

### S1 — Identity spine
**Build.** SPIRE deployed; sandbox gets a JWT-SVID. `steward-mint` exchanges SVID → HOP-1
carrying the acting user's email and an explicit acting-as marker. `mcp-gw` resolves the
user's provider credential by email and executes one tool call.

**Proves.** A tool call executes with **the user's** provider token, by a process that
never held it.

**Exit.** Positive: the agent lists the user's GitHub repos. Negative, all three must
fail closed — a forged/expired SVID, a HOP-1 for a different email, and a tool outside
`spec.tools`. Size: **L** — highest-risk integration in the system, which is why it is
second and not last.

---

### S2 — Inference and budget
**Build.** Per-runtime LiteLLM virtual key, minted at provisioning and delivered to the
OpenShell supervisor. Spend observed on a poll. Exhaustion drives `Running → Suspended`.

**Proves.** The one runtime-driven state transition works without a human. Also forces
the key-delivery decision by building it rather than deciding it on paper — provider-
profile credential bundle vs. token-grant exchange.

**Exit.** Set a $1 budget, run inference until it trips, observe `Suspended` and a dead
key. **Second exit criterion, non-optional:** a model with no registered cost is
**rejected at admission**. An unpriced model accrues zero cost and silently disables the
exhaustion transition — the budget looks enforced and is not. Catalog registration is a
control, not hygiene. Size: **M**.

---

### S3 — Envelope admission
**Build.** Postgres arrives. Envelopes authored per member role, immutable revisions.
`steward-admission` computes admit / reject-with-deltas. Webhook wired. Approval queue
rendered in the admin UI (the mockup's approvals screen, against real rows).

**Proves.** Admission evaluates **absolute composed values**, not deltas.

**Exit.** The anti-ratchet test: a sequence of edits each individually inside the
envelope, whose composition is outside it, is rejected. Plus: the same manifest is
rejected identically through `kubectl` (hard deny, counterexample in the message) and
through the API (deltas, parked). Size: **L**.

---

### S4 — Escalation
**Build.** Over-envelope request → structured deltas → Jira filed pre-populated → manifest
parks `Pending` → admin approves in Steward with the Jira link as evidence → `grants` row
written, bound to the runtime → provisioning proceeds. Resolution commented back to Jira.

**Proves.** Steward is state authority and Jira is a channel; the grant binds to the
instance.

**Exit.** After the approval, a **second** user in the same member role submits the same
over-limit request and is escalated exactly as the first was. The ceiling did not move.
Transitioning the Jira ticket does not change anything in Steward. Size: **M**.

---

### S5 — Revocation and teardown
**Build.** Suspend and terminate revoke the mint binding and the LiteLLM key together.
TTL expiry drives `Terminating`. Teardown traverses `status.refs`.

**Proves.** "A stopped agent holds no live access" — as an observed fact.

**Exit.** After terminate, all four fail: the mint refuses to issue for that runtime; a
previously-issued HOP-1 is rejected at `mcp-gw` after its short TTL; the LiteLLM key
returns 401; the sandbox is gone. No orphaned refs, verified by a reconciler sweep that
finds nothing. Size: **M**.

---

## 7. Explicitly out of v0.1.0

`Workflow` / `WorkflowBinding` / `Task` · the journal · the executor and step gate · the
data plane (git gateway, blob CAS, registry proxy, fetch proxy) · the knowledge layer ·
service principals beyond schema · `TaskTrigger` · the human↔agent interactive plane ·
per-team and per-cost-center envelopes · multi-cluster.

**Two exceptions, both schema-only and both cheap now:** the `Principal` enum's service
arm (§4), and the reserved `bindings` field (§5).

### 7.1 The human↔agent plane — deferred, with one decision taken now

Tier 0 is default-deny egress: the sandbox cannot open a connection to Slack or to a
browser. So an interactive stream must traverse a **Tier-1 stream relay** — a fan-out
with a per-subscriber entitlement check, resolved on the email join key. It is a Tier 1
block by the membership test: state no single sandbox can see, authorization per acting
user.

Deferred entirely. One decision is taken now because it is schema-shaped and cheap:

> **The sandbox→relay stream is a typed event stream** — `token`, `tool_call_start`,
> `tool_result`, `turn_end`, `parked_for_approval`, `session_end` — not raw provider SSE.
> This makes recording a single tap at the relay (not inside the sandbox, which would
> break role-implementation purity; not per-frontend, which fragments it) and makes
> frontends adapters over one contract.

The consumer side of that contract — granularity, backpressure, resume, entitlement
re-checks — is `SessionRelay`, and it is specified in §2.6.3 even though nothing
implements it in v0.1.0. Terminal-like adapters are streaming consumers by nature, and
the contract is cheaper to cut once.

And one constraint that follows: **Steward builds no chat adapter.** Rendering into a
conversational surface is narration, which belongs to a connector (§2.6.7). Steward builds
the relay and the web/TUI adapters; a connector subscribes — Burble does this for Slack.
Build a chat adapter into the core and a second, ungoverned egress path appears alongside
it by default.

---

## 8. Foundation posture — the schedule

OpenShell is at `v0.0.82`, releasing near-daily, with the isolation model still landing.
Steward's product claim is a *security* claim, so the risk that matters is not that the
foundation moves — motion is loud and CI catches it. It is that a guarantee we sell
quietly stops being provided by the mechanism we believe provides it.

The two most expensive corrections in this project so far were both about a foundation
standing perfectly still: `mcp-gw` has no per-agent grant object, and the per-agent model
allowlist is not expressible at Tier 0. Neither was breakage. Both were wrong beliefs
about static behaviour, and one was caught by a third party before us.

So the posture is inverted from the instinct. The concrete goes at the **top**:

| Layer | Posture |
|---|---|
| CRD schema, governance objects, API contract | **Freeze hard.** Versioned, deprecation windows, migrations. This is what "castle" means |
| Admission, envelopes, escalation, ledger, portal | **Build well.** Outside OpenShell's plausible scope — §8.5 |
| `adapters/*` | **Deliberately thin and swappable.** All churn lands here, on purpose — and class C adapters are replaceable outright (§2.5) |
| OpenShell, `mcp-gw`, LiteLLM | Pinned, conformance-tested, negotiated upstream |

> **Invariant.** OpenShell semantics do not leak out of `adapters/openshell`. If they
> reach `steward-admission` or the controller's state machine, the absorbing layer has
> failed and RFC 0005 becomes a refactor of everything instead of a rewrite of one crate.

### 8.1 The guarantee register

Every property Steward sells, the mechanism providing it *today*, and an executable
negative test. The tests assert **their** behaviour, not ours: attempt the violation,
then assert the outcome.

**The register is split, and the status column is generated.** Prose — what we sell, the
mechanism, the watch item — is authored in `conformance/register.toml`. Status is
**derived from the test run** by `cargo xtask register`, which refuses to render a claim
the evidence does not support. A hand-typed status column drifts, and the two invariants
resting on it (nothing sold that the register does not mark `provided`; nothing marked
`provided` without a green negative test) are unenforceable while a human types the word.
Design: `docs/guarantee-register-generation.md`.

Two test directions, one module per guarantee:

- `holds_*` — the violation must fail. Red is a **regression**.
- `gap_*` — the violation currently succeeds, and the test says so. Red means the **gap
  may have closed**: upstream improved and we are now under-selling. That is the good
  kind of finding, and most registers cannot detect it.

The table below is generated. Do not edit it here.

<!-- BEGIN:generated-register -->
*(generated by `cargo xtask register`; the block below is a worked example of the
intended output, replaced on first run)*

| # | Guarantee sold | Mechanism today | Status (derived) | Pinned | Latest | Evidence | Watch |
|---|---|---|---|---|---|---|---|
| **G-1** | Agent egress reaches only authorized destinations | supervisor, default-deny L7 | `provided` | ✅ | ✅ | 4 holds | RFC 0005 (#2155) restructures the mechanism; verify the property, not the mechanism |
| **G-2** | An agent cannot obtain another user's credentials | HOP-1 claims + `mcp-gw` email resolution | `provided` | ✅ | ✅ | 5 holds | #1970 may replace the SVID→HOP-1 leg entirely |
| **G-3** | An agent's authority is re-verified at most every T seconds | short HOP-1 TTL; re-mint re-evaluates | `provided` | ✅ | ✅ | 2 holds | T is configuration (`STEWARD_AUTHORITY_TTL`, default 60s), not a measured latency (D8) |
| **G-4** | A terminated agent holds nothing live | mint revocation + LiteLLM key delete | `provided` | ✅ | ✅ | 4 holds | mostly ours; the S5 exit criterion is this module |
| **G-5** | An agent reaches only its allowed models | **LiteLLM per-agent key — not Tier 0** | `partial` | ✅ | ✅ | 3 holds | inferred wrongly once; re-verify whenever the inference router changes |
| **G-6** | Agents are isolated from each other | **name-based only** | `not yet provided` | ✅ | ✅ | 2 gap | Phase 2 (RFC 0011). A red `gap_` here means upstream closed it |

Pinned: `v0.0.82` · commit `—` · run `—`
<!-- END:generated-register -->

**G-6 is the live example of the whole problem.** Upstream's own words: isolation is
name-based, and any authenticated user who knows a workspace name can operate in it. If
S0.0 concludes workspace-per-`AgentRuntime` is the right shape, we would be resting a
per-agent isolation claim on a mechanism that does not isolate — and it would look
entirely correct in the architecture diagram.

A register entry with `not yet provided` is not a defect. Selling G-6 before the status
column says `provided` is — and now the column cannot say `provided` unless a green
`holds_` test says so first.

**Cost to name honestly:** the suite needs a real environment — kind/k3s, a pinned
OpenShell, SPIRE, a `mcp-gw` instance, LiteLLM. That environment is an S0.0 deliverable
and is reused by every slice's exit criteria.

### 8.2 The cadence

| Rhythm | What | Cost | Output |
|---|---|---|---|
| **Per upstream release** | Conformance suite runs against `latest` in CI. Pinned lane must stay green and blocks merge; latest lane is informational and alerts | automated | A red test is a finding, filed with its G-number |
| **Weekly** — upstream watch | Re-verify the tracked set via the authenticated GitHub connector, never web search. Delta only: our lanes, RFCs in flight, threads we are cc'd on | ≤1h | One line per item appended to a running log |
| **Biweekly** — contribution slot | A *booked* block, not "when there's time." One comment or one PR, ordered by the lane priority in the strategy doc | ½ day | Something posted upstream |
| **Monthly** — upgrade gate | Decide the pin: move / hold / hold-with-patch. Review patch debt. Absorb breaking changes deliberately | 2h + the upgrade | A recorded decision, with reasons, even when it is "hold" |
| **Per slice** — coupling checkpoint | §8.3 | in the slice | Register updated, dependencies recorded |
| **Quarterly** — strategy re-draw | Re-survey the full active surface. The strategy doc already went stale once by being too narrow; schedule the re-survey rather than discovering it | 1 day | Strategy doc revision |

Roughly 5–6% of one engineer, sustained. Worth stating so it is planned rather than
squeezed, because the first thing that gets squeezed is the weekly watch, and the weekly
watch is what makes everything else cheap.

### 8.3 Per-slice coupling checkpoint — a gate on slice exit

The schedule attaches to the roadmap rather than running beside it. No slice is declared
done until:

1. Every guarantee that slice depends on has been **re-run green** against the pinned
   release — not assumed from the last run.
2. Every upstream thread the slice's design now depends on is **recorded in the register**
   with its issue number. A dependency nobody wrote down is the mis-inference risk with
   extra steps.
3. Any belief about upstream behaviour formed *during* the slice is either backed by a
   test or marked unverified. "We read the code and it seemed to" is a finding, not a
   fact.

Slice-to-guarantee map: S0 → none. **S1 → G-2.** **S2 → G-5.** S3, S4 → none (ours).
**S5 → G-4**, and G-1 regression. G-3 is verified at S3 when bundle push first carries
real policy. G-6 is verified whenever workspace-per-agent is decided, and gates nothing
until it is.

### 8.4 Pin, patch, upgrade

**Pin an exact release**, recorded in one place, referenced by CI and Helm. Two lanes:
pinned (must be green, blocks merge) and latest nightly (informational, alerts).

**Upgrade on a schedule with a gate, never on release.** One upgrade window per month
maximum. Never in a slice's final week, and never upgrade and open a slice in the same
week — you lose the ability to attribute a failure.

**Patch debt, three rules:**

1. **Never carry a patch that has not been attempted upstream.** The attempt — issue,
   comment, or PR — is recorded alongside the patch.
2. **Every patch has a stated exit condition:** the upstream item whose landing removes
   it.
3. **Patch set size is a tracked metric**, reported at the monthly gate. Shrinking means
   the strategy is working. Growing is a slow-motion fork nobody decided on.

Apache-2.0 makes forking legal, not cheap. The useful escape hatch is "we can run a
patched build," not "we can maintain a fork."

### 8.5 Where the moat is, and where it is not

Every design decision gets one question: **is this in OpenShell's plausible future
scope?**

- **Yes** → thin it, isolate it, expect to delete it. #2109 is the live case: if managed
  maximum policies land with the subagent expansion, part of our admission layer becomes
  redundant. That is a *good* outcome, and only available if `steward-admission` is a
  separable crate rather than conditionals threaded through the controller.
- **No** → build it properly. OpenShell is a sandbox runtime. It will not ship a Jira
  integration, an enterprise approval queue carrying structured deltas, a spend ledger, a
  self-service portal, or an envelope editor with blast-radius visualisation. That list
  *is* Steward, and none of it is exposed to their churn.

Worth saying plainly: **the containment proof was never the moat.** Better to know that
before someone builds it as though it were.

### 8.6 Out-of-band triggers

These jump the cadence. Any one of them takes the next available slot regardless of
what was booked:

- **RFC-0011 Phase 2 PR opens.** We are owed a cc. Positions are pre-written (Lane F), so
  review is same-day — that promise is worth more than anything else on the list and it
  expires if we are slow.
- **A conformance test goes red.** Triage before the next slice task.
- **#1970 merges** — decide whether the SVID→HOP-1 leg becomes the native two-stage
  exchange (affects G-2 and the mint).
- **RFC 0005 merges** — G-1's mechanism has changed; re-verify the property, not the
  mechanism.
- **#2109 / #2168 moves** — re-evaluate the `steward-admission` boundary.
- **`/vouch` granted** — Lane G opens; take the smallest real item first.
- **A breaking change lands near our pin** — pull the upgrade gate forward.

### 8.7 Upstream engagement, re-filed

The strategy doc treats engagement as credibility and standing. It is that, and it is
mis-filed. **It is dependency risk management**, and should be budgeted as such — which
is why the contribution slot is in the table above rather than in a separate document.

- **#1613** — publishing the gRPC surface as a supported integration contract is the
  highest-value item in the repository for us. It converts our largest coupling from an
  undeclared surface into a promise.
- **Phase 2 review access** means we see *semantic* motion before it lands. Semantic
  motion is the dangerous kind: the code still compiles and the meaning changed.
- Every guarantee written down upstream is a place the foundation solidifies specifically
  under us.

The way to stop a foundation moving under you is to be one of the people deciding where
it moves. That is not aspirational — the Phase 2 cc is already committed.

### 8.8 The thing to say to leadership

`v0.0.82`. They are telling us it is pre-beta and the release cadence confirms it.
Steward v0.1.0 is a PoC on a pre-1.0 dependency, and **Steward's GA timing is coupled to
OpenShell's** whether or not anyone says so out loud. Say it now, with the conformance
suite, the pin policy, and the upstream lane as the mitigation — rather than have it
surface during a customer conversation.

---

## 9. Risk register

| # | Risk | Handling |
|---|---|---|
| R1 | OpenShell gRPC is not a declared stable contract (#1613) | S0.0 spike; thin pinned client; integration test against a pinned release; treat upgrades as scheduled work |
| R2 | #2243 breaking changes — 19-char names, required labels, drain-before-upgrade | Absorbed in S0.0; naming scheme sized to the cap |
| R3 | Trace context is dropped on every managed inference call | No end-to-end trace correlation until #1758 lands. Correlate on runtime UID in structured audit meanwhile. This is also our own Lane G PR |
| R4 | LiteLLM key delivery mechanism unverified | Resolved by building S2; spike the provider-credential path first, do not design it on paper |
| R5 | Unpriced models silently disable budget exhaustion | Admission-time rejection, S2 exit criterion |
| R6 | Email join key wrong → wrong user's access | The worst failure in the system. Verify that every channel's email ≡ SSO email ≡ `mcp-gw` credential key **before** S1. Every connector inherits this; none of them owns it |
| R7 | #2109 managed maximum policies may replace part of our admission layer | Good outcome. Track deliberately; keep `steward-admission` a separable crate so the containment proof can move upstream and leave envelope authoring, parking, spend, and the portal with us |
| R8 | Per-agent model allowlist not expressible at Tier 0 | Closed, and now closed in both directions. Enforcement is the LiteLLM per-agent key alone. Routes became workspace-scoped in #2243 but B1 stands — a route pins one model, not a set — so Tier 0 cannot express a per-agent *or* per-team model list. Workspace-per-agent is rejected (D1) |
| R9 | Postgres becomes a second source of truth for phase | Single-writer discipline, §3. Status is a cache; Postgres never holds current phase |
| R10 | Product rename touches the API group | §2.3 — keep the name out of the group |
| **R11** | **A guarantee we sell stops being provided, silently** | **The register and suite, §8.1. This is the risk R1–R3 are only the loud subset of** |
| **R12** | **Upstream cadence gets squeezed when a slice runs late** | **The slots are booked, §8.2. The weekly watch is the first thing cut and the thing that makes everything else cheap — cut the biweekly slot before it** |
| **R13** | **Patch debt becomes an undecided fork** | **§8.4: no patch without an upstream attempt, every patch has an exit condition, set size reported monthly** |
| **R14** | **A port turns out to be vendor-shaped — one implementor, no portability, indirection for nothing** | **§2.5.1 derives ports from guarantees, and `adapters/fake` is a real second implementor. Review the `InferencePlane` port hardest: LiteLLM does three jobs and the port must not assume one system does all three** |
| **R16** | **Algorithm confusion at the HOP-1 verifier** | **Low today and implicit: `jose` will not use an asymmetric public key for an HMAC verify, so RS256→HS256 fails. But `jwtVerify` is called with no `algorithms` allowlist, so the protection is a library property rather than stated intent. Upstream a three-line fix to `mcp-gw` deriving the allowlist from the issuer profile, and cover it with a G-2 negative test** |
| **R15** | **A swapped adapter silently weakens a guarantee** | **§2.5.2: adapters declare capabilities, admission refuses rather than degrades, and the register derives status from what the configured adapter can actually enforce** |

No calendar estimate is given. S0.0 determines it, and quoting dates before reading the
gRPC surface would be inventing them.

---

## 10. Open decisions carried

**Status.** Sixteen of seventeen resolved or ratified. D11 is open by design. D17 is parked
until a customer deployment. D16 was opened and closed during the same review — narrowing an
envelope beneath a running agent had no defined behaviour at all.

**Naming.** Slices are `S-1`…`S5`. Decisions are `D1`…`D17`. They collided in an earlier
draft and the collision was live — §8.1 read "measured in S3 … (S8)" with one meaning a
slice and one a decision.

| # | Decision | Resolution | Cost of deferring | Blocks |
|---|---|---|---|---|
| **D1** | **RESOLVED — workspace-per-`AgentRuntime`** | **No. `Team = workspace` (*Data Plane Spec* §6); envelopes layer within. Per-agent workspaces would be the third boundary both that spec and *Upstream Strategy* position 4 refuse. The one argument for it — per-agent model routing at Tier 0 — fails on its own terms: routes are workspace-scoped since #2243, but B1 stands and a route pins *one* model, while `spec.llms` is a list. What remains is blast radius without isolation, since isolation is name-based until Phase 2** | — | **S0.0, S0** |
| **D2** | **RESOLVED — LiteLLM key delivery** | **Token-grant exchange, with the Steward mint as the `token_endpoint`. The supervisor pulls a per-sandbox JWT-SVID (`ClusterSPIFFEID` from `openshell.io/sandbox-id`), exchanges it at the mint, and injects the returned per-runtime virtual key — nothing is delivered into the sandbox. Revocation becomes a property of the mechanism, attribution is structural, and it reuses S1's exchanger. The #1970 uniform-scope concern applies to third-party endpoints, not to one we operate: what returns is derived from the SVID identity, not the requested scope** | — | **S2** |
| **D3** | **RATIFIED — session recording granularity** | **At the relay, one tap, typed events. Not inside the sandbox (that breaks role-implementation purity), not per-frontend (that fragments it). Access to recordings is a **separate authorization path** from live subscription — see D4 point 5** | deferred with the plane | — |
| **D4** | **RESOLVED — break-glass admin access** | **Yes, and defensible because it is loud. (1) A distinct named operation, never an admin role that quietly passes the entitlement check — otherwise there is no line between routine and emergency and no signal that anything happened. (2) The acting user is **always** notified, immediately; this is the property that makes it acceptable. (3) Recorded in `runtime_events` with a reason, visible in the fleet view. (4) Time-bounded to T; continuing means re-invoking, which re-notifies. (5) **Never retroactive** — a live subscription only. Recordings are a separate authorization path, because "watch during an incident" and "read everything this person's agent has done" are different powers. The requirement is narrower than it looks: most incident response is *suspend or terminate*, which needs no session access at all** | deferred with the plane | — |
| **D5** | **RESOLVED — envelope granularity** | **Member role only in v0.1.0. Schema admits more without implementing it: key `envelopes` on `(scope_kind, scope_ref, revision)` with `scope_kind` restricted to `member_role` — a column now instead of a migration plus an admission-query rewrite later. Composition rule stated now, unimplemented: **envelopes intersect, never union.** A team envelope can only narrow. Widening is what escalation and instance-bound `grants` are for** | — | **S3** |
| **D6** | **RESOLVED — HOP-1 signing algorithm** | **EdDSA (Ed25519). The premise for RS256 was false: `mcp-gw`'s `validateHop1Jwt` passes no `algorithms` option to `jwtVerify`, so it accepts whatever the trusted issuer's JWKS advertises. Ed25519 costs zero verifier changes. `validateHop1JwtForIssuers` already iterates issuers with independent JWKS, so the Steward mint is added beside the proto-mint with a different algorithm — no cutover. Algorithm is mint-side config as a closed enum (`Ed25519 \| RS256`), never a free string, never `none`, never an HMAC family** | — | **S1** |
| **D7** | **RESOLVED — `steward-admission` vs #2109** | **Split by role, decided in advance. **Containment** (manifest vs. ceiling — pure, stateless, no Steward concepts) is *deletable*: if #2109's managed maximum can express our four envelope dimensions (models, tools, budget, TTL), we delete ours and call theirs. **Composition and consequence** (envelope revisions, structured deltas, parking, instance grants, the queue) never goes upstream — it is the product. If containment moves, G-1/G-5 mechanism lines change from ours to upstream's and gain conformance tests, because we would then depend on a behaviour rather than implement it. Build nothing toward it: a separable crate is deletable, and that is all the preparation an unmerged draft warrants** | — | — |
| **D8** | **RESOLVED — authority re-verification interval** | **G-3 restated: not "a policy change propagates within N seconds" but **an agent's authority is re-verified at most every T seconds.** The enforcement clock is the HOP-1 TTL — a number we set, not a latency we measure. `STEWARD_AUTHORITY_TTL`, default **60s**, with the token-grant access-token cache aligned to it so the two clocks cannot disagree. Revocation does not wait for T: suspend and terminate delete the LiteLLM key and refuse at the mint immediately (G-4, different mechanism, faster)** | — | **S3** |
| **D9** | **RATIFIED — publishing the conformance suite** | **Not in v0.1.0. Leverage in the #1613 conversation later — an integrator's conformance suite is close to what RFC 0014 is reaching for. Kept publishable by default: neutral by rule (`AGENTS.md` §12) and generated with provenance, so publishing is a render rather than a project** | **low — pure upside, no urgency** | — |
| **D10** | **RESOLVED — `main` vs `master`** | **`main`. Greenfield repo, git's default since 2.28, what every tool assumes. Other repos are not renamed; each repo's `AGENTS.md` states its own branch and the nearest file wins** | — | **S-1** |
| **D11** | **OPEN BY DESIGN — push escalation for the biometric-key socket failure** | **Unresolvable in advance, deliberately. Guessing the command produces exactly the retry loop `AGENTS.md` §1.3 exists to prevent. Record it there the first time someone resolves it** | **low per occurrence; recurs until written down** | — |
| **D12** | **RESOLVED — shared/manual DEV** | **DEV exists (AWS, one EC2 per product, Terraform + Ansible + docker compose) and holds real credentials. Steward cannot use it as-is: no API server means no CRD, no operator, no admission webhook. Steward's DEV is Kubernetes — **k3s or kind on the existing EC2** first; EKS when there is a customer deployment to mirror. Test infrastructure stays local-ephemeral regardless (§5). The §5.4 guard becomes mechanical in S-1: the harness asserts the kube context matches the local pattern and refuses otherwise** | — | **S-1** |
| **D13** | **RESOLVED — which ports exist in v0.1.0** | **all eight, defined in S-1. Five reach `proven`, three stay `provisional` (§2.5.4). Traits are not versioned; the two wire contracts that cross a boundary are** | — | **S-1** |
| **D14** | **RESOLVED — a second OpenShell adapter** | **No. Class B is a deliberate bet. The seam bounds the blast radius to one crate if it ever needs unwinding, but nothing is built toward portability we are not buying — an ambiguous "maybe" invites a half-built abstraction that constrains the design without ever being exercised (R14). If OpenShell is abandoned, we rewrite `adapters/openshell`; §8 and the upstream lane are the mitigation, and §8.8 already says GA timing is coupled** | — | — |
| **D15** | **RATIFIED — GitHub as an input channel** | **Not in v0.1.0, and not until §2.6.5's principal resolution is built and tested. Our upstream work is on public repositories, so comment authorship is not identity: the adapter must resolve an authenticated org member with a verified corporate email mapping and reject everything else** | **high if built carelessly — R6 with an internet-facing front door** | — |
| **D16** | **RESOLVED — a running agent whose envelope is narrowed beneath it** | **Suspend at next re-verify (≤ T). The envelope edit carries `enforcement`: `immediate` (default — running agents outside the new envelope suspend) or `on_next_admission` (they continue; the narrower envelope binds new requests and any spec change), which requires a reason recorded in `runtime_events`. Suspension is not termination: state is kept and the agent resumes if the envelope is restored or an instance-bound grant is issued. The editor's blast-radius view must show the count of agents and owners a narrowing will suspend *before* the click, or admins will choose `on_next_admission` out of fear and defeat the default** | — | **S3** |
| **D17** | **EKS for Steward's shared DEV, and org-wide** | **Not now, and not coupled. k3s/kind on the existing EC2 gives a real API server, real CRDs, a real webhook and a real `helm install` at near-zero incremental cost — most of the parity that matters for a PoC. EKS adds IRSA, managed control plane, LoadBalancer services, storage classes and cert-manager DNS: deployment concerns that matter when deploying *for* a customer. Migrating the other products is a separate question; coupling it makes a platform migration a blocker for the PoC** | **low — revisit at first customer deployment** | — |

---

## 11. Invariants added

1. **v0.1.0 is Plane A.** Plane B objects appear in the schema as reserved fields and
   nowhere else.
2. **One fact, one writer.** Desired state in `spec`; current phase in `status`; history
   and queryable detail in Postgres. Status is a cache, never a source.
3. **Every writer of desired state passes the same admission library.** The webhook
   enforces; the API enforces and escalates. There is no third door.
4. **A granted exception is a row bound to a runtime UID.** It never edits an envelope.
5. **Join on UID, never on name.**
6. **Spend is observed from LiteLLM, never custodied by Steward.**
7. **The mint takes a `Principal`.** No interface in the system takes a bare acting-user
   email.
8. **Steward builds no chat egress.** Notification and narration reach conversational
   surfaces through `NotificationSink`, implemented by a connector (§2.6.7).
9. **Vendor semantics do not leak out of their adapter.** Core crates depend on
   `steward-ports` and nothing below it. Everything above the adapter boundary is
   written in our own vocabulary — including OpenShell's.
10. **Nothing is sold that the register does not mark `provided`**, and nothing is marked
   `provided` without a green negative test against the pinned release.
11. **No patch is carried that has not been attempted upstream**, and every patch names
   the upstream item that would remove it.
12. **A slice is not done until its guarantees have been re-run**, not assumed from the
   previous run.
13. **One adapter is fine; zero interfaces is not.** Any plane with more than one
   plausible vendor is reached through a port (§2.5).
14. **Channels carry decisions; they never make them.** An adapter returns a
   `DecisionIntent`, never a `Decision` (§2.6.4).
15. **Inbound is the API.** Steward grows no inbound adapter and polls no external
   system for work (§2.6.1).
16. **A stream never stalls the sandbox, and a dropped subscriber is told.** Coalescing
   happens in the relay, on a declared granularity; `Lagged(n)` is mandatory (§2.6.3).
