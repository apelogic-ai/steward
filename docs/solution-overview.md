# Self-Service Agent Provisioning — Solution Overview

Status: draft for internal review and convergence
Audience: engineering leadership, IT / security, platform, product
Companion document: *Development & Integration Spec* (implementation detail)

> **Naming note.** *Steward* is a **temporary working name** for the control plane
> described here, pending a final brand + availability/trademark check. It is not an
> OpenShell sub-brand: the product is built *on* OpenShell (NVIDIA, Apache-2.0) but
> named independently, to avoid trademark entanglement and the SEO collision with the
> unrelated Open-Shell Windows utility. Expect a find-and-replace when the name lands.

---

## 1. What we are building

A self-service way for an enterprise member to stand up a **long-running agent**
and have its access to inference, tools, and spend governed automatically —
inside limits their role already grants, and through a clean escalation path when
they ask for more.

The user experience is a portal request:

> "I want an **OpenClaw** agent, on **Claude Sonnet + a local model**, with
> **GitHub + Google Drive** tools, at **\$300 / month**."

If that fits what the user's role is pre-approved for, the agent provisions with
no human in the loop. If it exceeds the pre-approved envelope, an escalation opens
automatically, the request parks in a pending state, and an IT admin approves (or
not) with a documented rationale before anything runs.

Modifying an existing agent goes through the same door. The result is one portal
with **two faces**: a **user UI** (my agents, my spend, request / modify) and an
**admin UI** (approval queue, envelope authoring, fleet view).

The hard part is not the form. It is making the limits *real* — enforced at
runtime by something the agent cannot talk its way around — while keeping the
authoring of those limits in one place so the portal isn't lying about what it
governs.

## 2. The shape of the system

Four things, and one contract that ties them together.

**The contract is a manifest — an `AgentRuntime` resource.** Every request, and
every later modification, is a desired-state document: agent type, acting user,
LLMs, tools, monthly budget, TTL. This is the single object the portal writes, the
approval acts on, and the platform reconciles into reality. Everything downstream
is derived from it.

**Steward is the brain.** It admits or escalates requests,
composes each agent's effective policy, provisions the runtime, projects the
governing limits out to the enforcement points, mints the agent's identity, and
tracks spend and lifecycle. Crucially, Steward is an **extension of the OpenShell
gateway we already run**, not a new policy engine standing beside it — it inherits
OpenShell's policy store, its formal policy prover, and its bundle-push channel to
running sandboxes.

**The runtime is where the agent lives and where enforcement is absolute.** Each
agent runs in an OpenShell sandbox whose network supervisor is a default-deny L7
egress proxy. Nothing leaves the sandbox unless the agent's policy authorizes that
exact destination. This is the layer that makes "the agent may only reach the tool
gateway and the inference gateway" a *fact on the wire*, not a hope about network
topology.

**The shared gateways own what no single agent can see.** Two of them:
`mcp-gw` (our MCP tool gateway) holds per-user provider credentials, the tool
catalog, and tool-level policy; **LiteLLM** (on-prem) holds model catalog,
provider API keys, and — the reason it is non-negotiable — **budget and spend**
across every agent and every path. The org has already chosen to custody all
inference keys here, so LiteLLM is where monthly budgets are actually enforced.

```
   Self-service portal  (user UI | admin UI)
            │  writes / modifies
            ▼
      AgentRuntime manifest ──────────────┐  exceeds envelope → Jira escalation
            │  admit                       └── admin approves w/ evidence
            ▼
          Steward  ── composes policy · mints identity · projects limits · tracks spend
        ╱   │   ╲
       ▼    ▼    ▼
  OpenShell   mcp-gw      LiteLLM
   sandbox   (tools)    (inference)
  (Tier 0:   (Tier 1: shared, aggregate)
   per-agent,
   non-bypassable)
```

## 3. How a request flows

**Fits the envelope → auto-provision.** The portal submits the manifest. Steward
checks the requested spec against the ceiling the user's role is pre-approved for.
If every requested field is within the ceiling, Steward admits it, composes the
agent's policy, and provisions — no ticket, no wait.

**Exceeds the envelope → escalate.** If any field is over the line, Steward does not
guess and does not silently clamp. It computes the exact **over-limit deltas**
(this field, requested value, ceiling), files a Jira ticket pre-populated with
those deltas, parks the manifest in a **pending** state, and notifies the user.
Jira is where the humans discuss and decide. When the decision is to grant, an IT
admin approves the manifest **in Steward**, attaching the Jira link as evidence, and
provisioning proceeds. The granted exception is recorded on the manifest so the
agent keeps it — without quietly raising the ceiling for everyone in that role.

**Modifying an agent is the same flow.** A change is just a new revision of the
manifest through the same admission check. Widening a limit (more budget, another
model, a new tool) can escalate exactly like a first request; narrowing applies
immediately. Some changes reconcile into the running agent live (budget, model
list, tool grants); others (changing the agent type) require draining and
recreating it. The user sees which kind they're asking for before they confirm.

**Budget has a life of its own.** Unlike every other field, budget is both a
limit (set at admission) and a live measurement (spend, observed from LiteLLM).
When an agent exhausts its monthly budget, LiteLLM stops honoring its inference
calls and Steward reflects the agent as suspended and notifies the owner. Asking for
more budget is a widening change — it escalates like any other.

## 4. Where the limits actually live

A theme worth converging on explicitly: **the limits are authored once and
enforced in the place that can actually enforce them.** We do not re-type the
same rule into three systems.

The same role envelope is consulted at three different timescales:

- **At admission** — does this *request* fit? (auto-provision vs. escalate)
- **At provisioning** — compose the agent's effective policy from the envelope
  plus its specific grants.
- **At runtime** — is this specific tool call / model call / network destination
  allowed? Decided locally, per call, at the enforcement point.

And enforcement is layered, not scattered:

- **Per-agent, non-bypassable limits** live at the OpenShell sandbox: which
  destinations it may reach at all, which tools and endpoints, which models. This
  is the only layer the agent cannot route around, so anything security-critical
  lives here. It is also what forces every tool call through `mcp-gw` and every
  inference call through LiteLLM — closing a bypass gap we have documented in the
  current Burble MCP design.

- **Cross-agent, aggregate limits** live at the shared gateways: monthly spend and
  model catalog at LiteLLM; tool catalog, tool-level policy, and per-user
  credential custody at `mcp-gw`. A single sandbox cannot see org-wide spend or a
  shared catalog; these belong where that state actually lives.

Steward authors the envelope and *projects* it into each place in that place's native
form. The portal can honestly claim to govern the agent because every projection
traces back to the one authored envelope.

## 5. Identity and delegation

A long-running agent is, in plain terms, a **standing delegation of a person's own
access** — for a month, unattended. That deserves to be modeled deliberately.

- The **workload** (the sandbox) has a SPIFFE identity, issued by SPIRE. That is
  the root of trust for "this is a genuine OpenShell agent."
- Steward binds that workload, **at provisioning**, to the **acting user** — and mints
  the short-lived tokens the agent presents to the shared gateways, carrying the
  user's identity plus an explicit "acting-as" marker. The agent never holds the
  user's actual credentials; the gateways resolve those from the user's identity.
- The agent's granted tools and models are an **attenuation**: it gets a subset of
  what the user could do, never a superset.
- Because this is standing delegation, **revocation is first-class**. Suspending or
  terminating an agent revokes its identity binding and its inference key together;
  tool access then lapses on its own, because the tool gateway only honors the
  short-lived identity the agent can no longer obtain. A "stopped" agent must not
  still hold live access.

The user's email is the join key that ties their portal identity, their delegated
agent, and their stored provider credentials together. Getting that binding right
is what makes the whole thing safe.

## 6. Governance, evidence, and audit

- **Steward is the system of record for control.** State transitions —
  admit, approve, provision, suspend, terminate — happen in Steward, are authorized in
  Steward, and are audited there. Approval authority is *not* delegated to Jira
  workflow permissions.
- **Jira is the system of record for the conversation.** The escalation ticket
  carries the deltas, the discussion, and the decision. Steward files it, links it as
  evidence on the approved manifest, and comments the resolution back. Humans
  discuss in Jira; nobody re-types the numbers.
- **The numbers live in the manifest.** The over-limit deltas the admin sees, the
  reason a runtime call was denied, and the text rendered into the Jira ticket are
  the *same* structured data. This keeps the ticket, the policy decision, and the
  agent's actual limits from drifting apart.
- Runtime activity (tool calls, denials, inference) emits structured audit at each
  enforcement point, correlated by the agent's identity.

## 7. What this is not

- Not a general-purpose agent marketplace or a place to register arbitrary
  third-party MCP servers. The tool surface is the curated, governed set.
- Not a new policy language or a second policy engine. We reuse OpenShell's policy
  model and prover and the gateways' existing policy hooks.
- Not a replacement for the gateways. `mcp-gw` and LiteLLM stay as the shared tier;
  Steward governs and projects into them.
- Not a home for inference keys or provider secrets. Those stay custodied in
  LiteLLM and `mcp-gw` respectively; the runtime holds neither.

## 8. Decisions to converge on

These are the choices that change the build materially. The companion spec takes a
position on each; this section exists so we agree before we commit.

1. **Manifest as a real Kubernetes CRD with an Steward controller, or an Steward-owned
   object that is merely CRD-shaped?** A real CRD gives us Kubernetes-native audit,
   GitOps, and admission-webhook gating for free, and fits OpenShell's existing k8s
   driver — at the cost of committing the whole lifecycle to the operator pattern.
   *Spec position: real CRD + controller.*

2. **Provider-credential custody consolidating into `mcp-gw`.** `mcp-gw` now holds
   per-user provider tokens and resolves them by email. This moves custody out of
   the Burble app. We should confirm that consolidation and the email-as-join-key
   model, since it is load-bearing for correctness (wrong email → wrong user's
   access).

3. **Model allowlist: split or single-source?** Current position is a split —
   per-agent allowlist enforced non-bypassably at the sandbox, catalog + budget at
   LiteLLM. Single-sourcing it anywhere other than the sandbox loses
   non-bypassability. Confirm the split is acceptable operationally.

4. **Escalation channel = Jira, firmly.** Confirm Jira is the org's canonical
   channel and that Steward-files-async / admin-approves-in-Steward (rather than
   Jira-transition-auto-approves) is the accepted division of authority.

5. **Envelope granularity.** Per-role ceilings are the starting model. Do we need
   per-team or per-cost-center envelopes on day one, or can that wait?

## 9. Glossary

- **Steward** — our control plane (working name; see title note). An extension of
  the OpenShell gateway; the brain of this system.
- **Tier 0 / Tier 1** — non-bypassable per-agent enforcement (the sandbox) vs.
  shared aggregate enforcement (the gateways).
- **`mcp-gw`** — our MCP tool gateway (`apelogic-ai/mcp-gw`); tool catalog, policy,
  per-user provider credential custody.
- **LiteLLM** — on-prem inference gateway; model catalog, provider keys, budget.
- **Envelope / ceiling** — the pre-approved limits for a role that a request is
  admitted against.
- **`AgentRuntime`** — the manifest / desired-state resource for one agent.
- **Attenuation** — an agent receiving a strict subset of the acting user's access.
