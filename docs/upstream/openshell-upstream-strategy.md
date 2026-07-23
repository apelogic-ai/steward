# OpenShell Upstream Engagement — Strategy & Findings

Status: **v0.4** — engagement record, verified findings, and contribution plan
Audience: ApeLogic engineering
Upstream: `NVIDIA/OpenShell` (Apache-2.0)
Companions: *Solution Overview*, *Development & Integration Spec*
Last verified against live code and API: **2026-07-21**

> **Framing constraint.** Every upstream ask is expressed in terms of sandbox primitives —
> identity, policy scope, model sets, middleware phases, credential lifecycle, quota
> dimensions. None names our downstream choices. Portable asks get merged; asks that
> encode a vendor stack do not.

---

## 0. Where we stand

| Thread | State | Our position |
|---|---|---|
| **RFC-0011 multi-player** (#1980 → merged as #2243) | Phase 1 merged 2026-07-21 | Review comment posted; **maintainer committed to taking it into phase 2 and cc'ing us** |
| **Vouch request** (#2377) | Filed 2026-07-20, awaiting `/vouch` | Narrow ask (trace-context passthrough), references #1980 engagement |
| **#1970 SPIFFE token exchange** | Open, rebasing | Reviewed in full; **nothing to post** — see §6 |
| **#1755/#1756/#1757/#1758** credential-broker set | All open, all `state:triage-needed` since 4 June | **Our G2/G3 gaps, already articulated upstream** |
| **Trace-context PR (B3)** | Not started | Blocked on vouch; mechanism fully verified (§7) |

**Net:** we are in the review loop on the work that matters, without being vouched yet.

---

## 1. Engagement log

**2026-07-20 — Review comment on PR #1980 (RFC-0011).**
[Permalink](https://github.com/NVIDIA/OpenShell/pull/1980#issuecomment-5026761740). Three
points: (1) PR description/RFC body mismatch on policy layering, (2) the Workspace Admin
has no subtractive operation — no workspace-scoped deny, (3) delegation is attributable in
audit but not in the credential; proposed an optional `acts_for` claim.

**2026-07-20 — Vouch request #2377 filed.** Personal (`lbelyaev`), narrow: trace-context
header propagation through `inference.local` as a piece of #1758. Written first-person per
CONTRIBUTING; the template's self-certification checkbox forbids AI-generated text.

**2026-07-21 — #1980 closed; RFC merged via #2243.** `derekwaynecarr`:

> *"@lbelyaev I am working on enforcing the authorization elements as part of phase 2 now
> that the domain model got merged and rebased. I will take your comments into account as
> part of that effort, and cc you for your review."*

`grs` replied to point (3) by pointing at **#1970**, which implements user-subject token
exchange. Both comments received 👍.

**2026-07-21 — Full review of #1970 + #1987.** Read the live fork branch
(`grs:token-exchange`), not the API diff. Outcome: **four candidate findings, zero
postable.** Detail in §6.

---

## 2. Contribution policy (verified, `CONTRIBUTING.md`)

**Vouch system.** Rationale in their words: AI makes it trivial to generate
plausible-looking but low-quality contributions, so they no longer trust by default.

1. Open a Vouch Request discussion (category `vouch-request`).
2. Describe what you want to change and why.
3. **Write it yourself — AI-generated vouch requests are denied**, with a
   self-certification checkbox.
4. A maintainer comments `/vouch`.
5. Only then can you submit PRs. **Un-vouched PRs are auto-closed.**

**The gate is on PRs only.** Issues, issue comments, RFC review comments and discussions
are open to anyone — which is why the #1980 comment was possible before the vouch.

**Do not block on the vouch.** Two prior requests sat unanswered: one from 2026-06-09
(six weeks), and our org-level **#2345** from 2026-07-17.

**Where AI is welcomed vs forbidden — the distinction is provenance vs accountability:**
- **Vouch requests:** authorship asserted. Must be human-written.
- **Code/PRs:** no authorship rule; the bar is the **Critical Rule** — you must be able to
  explain the change without your agent open. Repeat offenders are blocked.
- **Issues:** agent output *encouraged* — the feature template has a dedicated **"Agent
  Investigation"** field, "optional but strengthens the proposal."
- **Comments/reviews:** no policy at all.

**Other gates:** copy-pr-bot vetting before CI runs on NVIDIA runners; DCO sign-off
(`git commit -s`); Conventional Commits; stale after 14 days inactivity (`roadmap` and
`state:triage-needed` exempt).

**Note on #2345.** Our org-level vouch names LiteLLM and agentgateway throughout, which
reads as "make OpenShell integrate with our stack" rather than "OpenShell is missing
primitive X". It is inconsistent with the vendor-neutral posture of everything else we have
posted. Worth an internal decision on whether to narrow it.

---

## 3. Repo state (verified 2026-07-21)

| Signal | Value |
|---|---|
| Open issues | ~263 |
| Open PRs | ~109 |
| Stars / forks | 7.7k / 1k |
| Release cadence | near-daily (v0.0.52 → v0.0.82, 29 May → mid-July) |
| Roadmap board | `NVIDIA/projects/233` |

Two cached snapshots minutes apart differed by ~600 stars and ~40 PRs. **Treat any repo
fact older than a week as stale.**

---

## 4. What landed: RFC-0011 phase 1 (#2243, merged 2026-07-21)

14,967 additions, 107 files, merged by `mrunalp`.

- **Workspaces** as isolation boundaries; `default` workspace preserves single-player
  behaviour; graceful `Terminating` deletion.
- **Workspace-scoped resources:** sandboxes, providers, service endpoints, SSH sessions,
  policies, settings, provider refresh state, **and inference routes**.
- **Membership model**; three roles defined (Platform Admin / Workspace Admin / User).
- **Provider profiles:** scope-aware layered catalog with workspace shadowing
  (`ProfileScope` = Static | Platform | Workspace, with `platform_fallback`).
- **Proto-driven authorization:** `AuthorizationRule` method options resolved at runtime.
- **Name uniqueness moved** from `(object_type, name)` to `(object_type, workspace, name)`.
- Service routing: `{workspace}--{sandbox}--{service}.{domain}`.

**Explicitly NOT enforced yet** (their words): *"Current isolation is name-based — any
authenticated user who knows a workspace name can operate in it, matching the pre-workspace
security posture where all authenticated users see everything."* `TODO(phase2)` markers sit
at each gap. **Phase 2 = the authorization work we are cc'd on.**

**Description/body drift, worth knowing:** the #1980 PR description claimed "three-tier
policy layering (gateway default → workspace baseline → sandbox policy)" with "allowlists
union across tiers." The RFC body describes **two** layers — gateway default and provider
profiles — and states *"there is no separate workspace-level policy to author or
maintain."* Our v0.2 concern about tier-union widening came from the description and was
wrong; §5's G0 is the corrected version.

**Also delivered:** inference routes are now **workspace-scoped** (`Cluster*` RPCs renamed
to `Route*`; workspace derived from the sandbox principal for bundle resolution). That
substantially addresses our old **B2**. **B1 stands** — a route still pins *one* model
rather than admitting a set.

**Explicitly out of scope in RFC-0011:** machine identity via OIDC workload identity
(future work — v0.2 had this wrong); multi-provider OIDC (non-goal; corporate SSO *and*
CI/CD OIDC simultaneously needs a follow-on — relevant to our Slack-plus-SSO case);
**OPA/Rego, deferred with an explicit invitation**: *"OPA/Rego authorization could be
layered on top of the workspace and role model in a future RFC if fine-grained policies are
needed."* That is where our envelope model lands.

---

## 5. Gap analysis

### Ours to raise (not yet upstream)

**G0 — Workspace Admin has no subtractive operation.** *(raised on #1980)* Effective policy
is gateway default + union of attached provider profiles; the only deny is gateway-level
and Platform-Admin-owned. A workspace-scoped prohibition is inexpressible — "team-ml must
never reach production-db, but team-ops must" cannot be stated. Provider curation is a
leaky substitute: because profiles union, declining one provider doesn't remove an endpoint
another profile also grants. Proposed fix: a workspace-scoped deny list owned by the
Workspace Admin. Adds no policy tier.

**G1 — Policy granularity is provider-shaped.** *(raised on #1980)* The finest expressible
grain is a whole provider profile. "Source-control provider, read-only" requires minting a
second provider — encoding an authorization distinction as a credential object. This is the
concrete requirement to bring to the deferred OPA/Rego RFC.

**G4 — Spend as a governed dimension.** Quotas cover concurrent sandboxes, GPU, and sandbox
lifetime — not monetary spend. Motivation cites cost attribution; Resource Governance
explicitly excludes chargeback. Spend is also structurally unlike the others: they are
declared and checked *before creation*, spend accrues at runtime and is knowable only from
the inference path. Observed, not declared.

**G5 — Over-limit requests are terminal.** *"Quota limits are hard — sandbox creation is
rejected when a quota is exceeded."* No park-pending → structured delta → approve-with-
rationale path. The structured delta is the load-bearing part.

**G6 — Teardown completeness.** Workspace deletion is rejected while sandboxes exist and
the namespace is removed after, but nothing requires that outward projections leave a
reference so teardown is a traversal rather than a sweep.

### Already upstream — do not re-file

**G2 — Scope attenuation → [#1756](https://github.com/NVIDIA/OpenShell/issues/1756).**
Filed 2026-06-04 by `dbora-nv`, open, `state:triage-needed`. Their framing is better than
ours: *"Attenuation is what makes the broker pattern actually implement least-privilege
delegation rather than just credential isolation."* Design: `scope_policy` per brokered
host/provider/tool/binary; request only the declared scope set for the calling identity;
structured escalation errors; provider-specific permission models (Graph scopes, GitHub App
permissions) rather than forcing everything into OAuth scope strings.

**G3 — Standing delegation for unattended agents →
[#1757](https://github.com/NVIDIA/OpenShell/issues/1757).** Same author, same day, same
state. *"Always-on agents may need explicit user authorization when the user is not actively
in session… the practical choices are fail the task, require an active session, or use an
over-privileged service account. None preserve user attribution and isolation
simultaneously."* Proposes an async consent broker (CIBA or provider-specific step-up),
pending handles, suspend/resume, never returning token material to the agent.

Its investigation also sharpens what we found in code: the Providers v2 refresh strategies
are *"all machine-credential or pre-acquired-token shaped; none initiate user consent."* So
the refresh worker keeps a token **alive**, but cannot obtain **new** consent. That is the
true residual gap.

**Parent: [#1755](https://github.com/NVIDIA/OpenShell/issues/1755)** — tracking:
generalize brokered credential delivery beyond `inference.local`. Also
**[#1758](https://github.com/NVIDIA/OpenShell/issues/1758)** — OTel trace correlation
(our B3's home). **All four filed 4 June, all still untriaged seven weeks later.**

---

## 6. #1970 / #1987 review — method and outcome

**#1987** (design) and **#1970** (implementation, `grs`) deliver user-subject token
exchange: a two-stage RFC 8693 flow where the gateway derives the intermediate audience
from the validated supervisor SVID, and the final token carries **the user as `sub` and the
sandbox SPIFFE ID as `azp`**. That is our HOP-1 design, arriving natively — third
independent convergence in this project after the two-tier model and the durable-orchestrator
split.

We drafted four review points. **All four were withdrawn:**

| Candidate finding | Basis | Outcome |
|---|---|---|
| Provider lookup lacks workspace scoping | stale API diff | **False** — head already passes `&workspace` (`provider.rs:2518`) |
| Cache key omits scopes | #1987 issue text | **False** — `token_cache_key()` includes `scopes.join(" ")`, with two tests |
| No per-sandbox revocation | inference | **Too strong** — `spec.providers` is per-sandbox and the handler enforces it |
| No refresh path for long-running sandboxes | inference | **False** — `provider_refresh.rs` has a full worker with `oauth2_refresh_token` and rotation persistence |
| Scope attributes but doesn't attenuate | traced code path | **True, but already filed as #1756** |

**Nothing was posted.** Two of the four would have been publicly wrong in a repo whose
contributing guide exists specifically to filter plausible-looking AI output.

---

## 7. Verified technical findings

### Inference request-header filtering (B3's mechanism)

**Single layer, confirmed.** `openshell-supervisor-network` depends on `openshell-router`
(`Cargo.toml`), and `tests/system_inference.rs` documents the chain as "route selection →
`proxy_with_candidates()` → mock backend → response":

```
sandbox → inference.local
  → openshell-supervisor-network  (L7 intercept, src/l7/inference.rs)
  → route selection → ResolvedRoute
  → openshell_router::proxy_with_candidates()
  → header filter (backend.rs)     ← the change point
  → upstream backend
```

**The filter** — `crates/openshell-router/src/backend.rs`:

```rust
let name_lc = name.to_ascii_lowercase();
if should_strip_request_header(&name_lc) || !allowed.contains(&name_lc) {
    return None;
}
```

Two gates, deny-by-default. `allowed` is `ResolvedRoute.passthrough_headers`, sourced from
`InferenceProviderProfile.passthrough_headers` — `&'static [&str]` constants in
`crates/openshell-core/src/inference.rs`, resolved in `openshell-server/src/inference.rs`.

| provider_type | passthrough_headers |
|---|---|
| `anthropic` | `["anthropic-beta"]` |
| `openai` | `["openai-organization"]` |
| `google-vertex-ai` (anthropic_messages) | `["anthropic-beta"]` |
| `nvidia` | `[]` |
| unknown / `None` | `[]` |

1. **Compiled in, not operator-configurable** — a Rust constant; no setting extends it.
2. **Every entry is a provider-API feature flag**; nothing observability-related exists.
3. **`traceparent`/`tracestate` appear nowhere in the repo** (code search: 0 hits). Trace
   context is dropped on every inference call, for all providers.

**Constraint a fix must respect:** `should_strip_request_header()` runs *first* and can
strip a header that is in `passthrough_headers` — `architecture/google-vertex-ai-provider.md`
documents exactly that for `anthropic-beta` on Vertex rawPredict routes.

**Positions to raise proactively:** trace context is *cross-cutting, not provider-specific*,
so attaching it to per-provider profiles is arguably the wrong shape — a global trace-header
passthrough independent of `InferenceProviderProfile` fits better. And because OpenShell
markets this as a *privacy* router, forwarding `traceparent` upstream leaks internal trace
identifiers (and `tracestate` can carry vendor key-values exposing internal system names):
**opt-in, default-off**, with a case for restricting it to self-hosted backends.

**Precedent:** **#932** (merged 2026-05-04) shipped `x-request-id` middleware and its
Alternatives section calls W3C `traceparent` *"the right long-term direction"*, deferred
only as heavier, noting the two are not mutually exclusive. So B3 is a **known, deliberately
deferred next step**, not an unnoticed gap.

**Scope:** three crates (`openshell-core` mechanism, `openshell-server` resolution,
`openshell-router` filter). Test pattern exists at
`crates/openshell-router/tests/backend_integration.rs` (`proxy_strips_auth_header`).

### Gateway credential refresh (answers the standing-delegation question)

`crates/openshell-server/src/provider_refresh.rs`: `spawn_refresh_worker` sweeps all
refresh states on a ticker; strategies are `static`, `external`, `oauth2_refresh_token`,
`oauth2_client_credentials`, `google_service_account_jwt`, `aws_sts_assume_role`;
`mint_oauth2_refresh_token` persists rotated refresh tokens back into state material;
`apply_minted_credential` writes the new access token and `credential_expires_at_ms` into
the provider. Refresh is scheduled at `expires_at − refresh_before_seconds` (default 300s).

**Implication:** a stored subject token *can* be kept alive indefinitely gateway-side. What
it cannot do is obtain **new** consent — which is exactly #1757.

### Gateway workload attestation

Supervisor-to-gateway RPCs use a gateway-minted bearer token scoped to the sandbox ID. On
Kubernetes the gateway mints it only after **TokenReview validates the projected
ServiceAccount token, the pod UID matches the live pod, and the pod's controlling `Sandbox`
ownerReference matches the live Sandbox CR.** SPIFFE-shaped subject
(`spiffe://openshell/sandbox/{uuid}`). Gateway is also an OIDC RP for humans with scope
enforcement.

### Credential custody

Refresh loop runs on the gateway using control-plane-held state; the sandbox proxy injects
only short-lived access tokens; **static credentials never enter the sandbox**; tokens are
not written to sandbox filesystems; cached material is discarded when a sandbox is recreated
with a new UUID.

---

## 8. Ecosystem position

- **`kubernetes-sigs/agent-sandbox`** (SIG Apps) is a hard dependency of OpenShell's
  Kubernetes driver — `sandboxes.agents.x-k8s.io` must be installed before the Helm chart.
  It is a *workload primitive*: one stateful pod, stable identity, persistent storage that
  survives restarts, gVisor/Kata backends. Extensions ship **`SandboxTemplate`**,
  **`SandboxClaim`**, **`SandboxWarmPool`** — which map closely onto our agent-class
  catalog, request flow, and warm provisioning.
- **kagent** (Solo.io, CNCF Sandbox) is a *different layer* — agents as CRDs, mesh-based
  connectivity, Istio ambient/ztunnel identity, agentgateway for MCP/A2A semantics. The real
  comparison is OpenShell vs kagent, and the split is philosophical: **containment
  (in-pod supervisor, kernel-level, default-deny egress) vs connectivity (mesh-level)**.
  That is our cooperative/coercive line.
- **Our position:** `mcp-gw` is agentgateway-fronted, so ApeLogic straddles both ecosystems —
  NVIDIA at the runtime layer, Solo at the tool-gateway layer. Upstream asks must stay
  stack-neutral so neither side reads us as the other's proxy.
- **`kagenti`** (IBM) pursues SPIFFE/SPIRE agent identity — adjacent to our mint, worth
  watching for convergence.

---

## 9. Method: verification discipline

The single most important operational lesson of this engagement.

**Every claim derived from a description was wrong or overstated. Every claim that survived
came from tracing live code.**

Concretely, in one session:
1. Reviewed a "three-tier policy layering" model that **does not exist** — it was in the PR
   description, not the RFC body.
2. Drafted a workspace-scoping bug against a **stale API diff**; head already had the fix.
3. Flagged a missing cache-key field that **was present**, with tests.
4. Asserted no refresh path existed when a **full refresh worker** did.
5. Reached two design conclusions that were **already filed as issues** six weeks earlier.

**Standing rules:**
- Use the authenticated GitHub connector, never unauthenticated API or web search, for repo
  facts.
- For PRs from forks, read the **fork branch at head** (`grs:token-exchange`), not
  `NVIDIA:main` and not the API diff — diffs go stale between fetch and read.
- Before writing any review point, **search issues for prior art**. Two of our best
  observations were already open issues.
- Distinguish *verified* from *inferred* in writing, and post inferences as questions.
- The Critical Rule is the right standard for us too: if you cannot defend it unaided,
  do not post it.

---

## 10. Plan

**Immediate, no vouch required**
1. **Add agent diagnostics to #1758** — it carries `state:triage-needed` ("opened without
   agent diagnostics"). We have exactly what it wants: the confirmed request path, the
   allowlist location and values, and the strip-ordering constraint (§7). Second
   substantive public contribution, zero permission needed.
2. **Comment on #1756 and #1757** stating that we operate the always-on-agent case they
   describe. Weight behind untriaged issues, not duplicates. **Do not re-file.**

**On `/vouch`**
3. **Land B3** as an implementation of #1758 rather than a new proposal — small, precedented
   by #932, and raising the opt-in/privacy constraint ourselves.

**Ongoing**
4. **Be ready for the phase-2 cc.** G0/G1 land there; `derekwaynecarr` has committed to it.
5. **File B1** (per-principal model *set*) as a design issue linked to #2039, framed as the
   bounded generalisation of passthrough.
6. **Decide internally on #2345** — narrow it or let the per-account request carry the ask.

**Watch**
- #1970 merge (changes our §6), #1756/#1757 triage, phase-2 authorization PRs, #1719
  operator design, #2039.

---

## 11. Impacts on our specs

1. **§6 identity — shrink.** #1970 delivers SVID handling, two-stage exchange,
   gateway-derived audiences, and agent-never-sees-user-token. Our mint becomes: compose the
   scope set, hold the delegation record. SPIRE demotes from required to *an option*;
   primary is the gateway-minted sandbox JWT (already SPIFFE-shaped). OIDC workload identity
   is **future work in RFC-0011, not delivered**.
2. **§6.4 delegator rule — keep and emphasise.** Multi-provider OIDC being a non-goal means
   the Slack-plus-SSO case needs our layer regardless. #1757 is the upstream analogue.
3. **§5.1 / §5.4 + Overview decision 3 — soften the Tier-0 model-allowlist claim.** B1 has
   not landed; a route still pins one model. Workspace-scoped routes (#2243) address B2.
4. **Tenancy — adopt RFC-0011 workspaces** as the scoping substrate; our envelope layers
   *within* a workspace. Do not invent a third boundary.
5. **§2 CRD — align with the emerging `OpenShellSandbox` consensus** on #1719: governance
   object that *generates* `agents.x-k8s.io/Sandbox`; policies and providers as separate
   referenced CRDs.
6. **§10 durable context — express stores as persistent volumes** on the runtime Sandbox;
   agent-sandbox already provides storage surviving restarts.
7. **Credential delivery — resolved.** Gateway-held state, proxy-side injection, nothing at
   rest in the sandbox, cache discarded on UUID change.
8. **Catalog — evaluate `SandboxTemplate`/`SandboxClaim`/`SandboxWarmPool`** as substrate
   for the agent-class catalog and tier-3→4 promotion.

---

## 12. Artifact index

| File | Status |
|---|---|
| `rfc-0011-review-comment.md` | **Posted** 2026-07-20 on #1980 |
| `pr-1970-review-comment.md` | **Superseded — do not post.** All findings withdrawn (§6) |
| `openshell-upstream-strategy.md` | This document |
