We're building an enterprise provisioning and governance layer on top of OpenShell, so the workspace and policy model here is directly load-bearing for us. Three notes, against the RFC body.

**Description/body mismatch.** The PR description says "three-tier policy layering (gateway default → workspace baseline → sandbox policy)" with "allowlists use union across tiers." The body describes two layers — gateway default and provider profiles — and states there is no separate workspace-level policy to author or maintain. Worth reconciling before merge.

**The Workspace Admin has no subtractive operation.** Effective policy is the gateway default plus the union of attached provider profiles, and the only deny mechanism is gateway-level and Platform-Admin-owned. A workspace-scoped prohibition is therefore inexpressible — "team-ml must never reach production-db, but team-ops must" can't be stated, because the only deny applies to every workspace or none.

Provider curation is also leaky as a substitute. Because profiles union, declining to offer one provider doesn't remove an endpoint that another provider's profile also grants; set membership doesn't compose subtractively. A workspace-scoped deny list — same semantics as the gateway deny, owned by the Workspace Admin — would close this without adding a policy tier or changing the provider model.

(Related, and probably for the deferred OPA/Rego RFC rather than this one: because policy derives from whole provider profiles, "this provider, read-only" requires minting a second provider — encoding an authorization distinction as a credential object.)

**Delegation is attributable in audit but not in the credential.** The sandbox JWT carries `sandbox_id` and nothing about the human; the creating principal's subject is added to OCSF events instead. So a SIEM can reconstruct who was behind an action after the fact, but a downstream tool server or internal API sees only the credential and can't attribute at the point of decision.

For long-running sandboxes — in practice a standing delegation of a person's access — an optional `acts_for` claim carrying the creating principal's subject alongside `sandbox_id` would make that legible downstream, while leaving the supervisor's authorization surface a single sandbox UUID. It would also make "max sandbox lifetime" meaningful as a delegation bound rather than only a resource bound.

Two smaller items we'd rather file separately than expand here: spend as a governed dimension (Motivation cites cost attribution, Resource Governance explicitly excludes chargeback), and over-limit requests as a pending state carrying a structured delta rather than a hard rejection. Happy to open those as issues.
