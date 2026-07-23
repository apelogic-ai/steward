# SUPERSEDED — DO NOT POST

All findings in this draft were withdrawn after verification against live code on
`grs:token-exchange` at head. Two were false, one was too strong, and the one that held
was already filed upstream as issue #1756.

See `openshell-upstream-strategy.md` §6 for the outcome table and §9 for the method
lessons. Retained only as a record of what was checked.

---

Thanks for the pointer @grs — this is the shape I was hoping for. The two-stage exchange with the audience derived from the supervisor's SPIFFE ID is neat, and having the final token carry the user as subject with the sandbox as authorized party is exactly the property that makes delegation legible downstream rather than only in the audit log.

Three things from reading through it.

## 1. Provider lookup needs workspace scoping after #2243

`handle_exchange_provider_subject_token` resolves the provider with a bare name:

```rust
let provider = state.store.get_message_by_name::<Provider>(&req.provider).await?
```

#2243 moved name uniqueness from `(object_type, name)` to `(object_type, workspace, name)`, so a bare-name lookup is now ambiguous — and this one sits on the path that mints a credential. The sandbox is already fetched immediately above, so its workspace is in hand.

The attachment check (`spec.providers` contains `req.provider`) constrains it in practice today, but it compares names rather than workspace-qualified identities, so it wouldn't distinguish two same-named providers in different workspaces. Worth pinning explicitly rather than relying on the attachment check while the rebase happens.

## 2. Scope is per-profile, so the token attributes but doesn't attenuate

`token_grant_request()` takes `scopes` and `audience` from the profile's `token_grant`, which is platform- or workspace-scoped. Every sandbox attached to that provider therefore receives an identically-scoped token. The result says *who* the agent acts for, but not *what this particular agent may do* on their behalf.

For short-lived sandboxes that's probably fine. For a long-running agent it's the difference between a delegation and an attributed impersonation — the agent gets the user's full scope set for that provider, for as long as it runs.

RFC 8693 allows `scope` to be narrowed per exchange, and #2243 just established a layered profile model, so the pieces are there: let the sandbox's effective policy intersect the profile's scope set (narrow only, never widen). That would make the exchange express attenuation rather than only attribution, and it composes with the phase-2 authorization work rather than competing with it.

## 3. Standing delegation vs. interactive delegation

#1987 says stored subject tokens *"should expire according to their token expiry"*, which is the right posture — the delegation shouldn't outlive the credential it was derived from. Following that through raises a case I don't think this flow covers yet.

The delegation's lifetime becomes the user's OIDC access token lifetime — typically minutes to an hour — and the refresh path is a human running `provider update --from-oidc-token`. That fits interactive use, where the user is present.

It doesn't fit a long-running agent. A sandbox provisioned to run unattended for days is, in practice, a standing delegation of a person's access; when the stored subject token expires there's nobody to re-authenticate, and the agent's calls start failing mid-task.

The same property also determines revocation granularity. Because the delegation is expressed as *"this stored credential exists and is attached"*, severing one sandbox's delegation means rotating or deleting the credential — which affects every sandbox attached to that provider. There's no per-sandbox delegation to revoke.

Both follow from the same root: the delegation is carried by a stored session token rather than by a delegation record bound to the sandbox. A purpose-scoped grant issued at provisioning, bounded by the sandbox's lifetime and individually revocable, would address both — though that's clearly a larger change than this PR.

Not asking for it here. But if user-subject exchange is going to be the mechanism for agents that run unattended, it's worth knowing whether that's in view, because it changes what the stored subject credential needs to be.

## Small: cache keying if scope becomes per-sandbox

#1987 lists the exchange cache key as provider, endpoint, audience, grant type, subject-token credential, and provider revision. If per-sandbox scope narrowing lands (point 2 above), the scope set needs to join that key — otherwise a narrowed token can be served from cache to a request expecting the profile's full scope.

---

Happy to be useful on any of these — the workspace-scoping one looks like a small fix worth folding into the rebase.
