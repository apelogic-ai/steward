# Steward browser-to-Identity HOP-1 attestation contract v1

This is the internal-only bridge from a verified Steward browser session to a short-lived
MCP-GW HOP-1 bearer. It is not a browser login route and is disabled unless its complete
configuration is present.

## Boundary

1. The existing browser-session middleware verifies the opaque session, resolves its Steward
   canonical user ID, and checks the normal same-origin/CSRF boundary on Connections routes.
2. The MCP-GW Connections broker asks `BrowserHop1AttestationIssuer` for a bearer. Its type
   accepts only `ConnectionSession<BrowserSessionBinding>`; it cannot accept a browser-provided
   subject, role, bearer, session string, or operation.
3. Steward signs a one-time ES256 assertion and calls Identity's private workload listener at
   `POST /v1/browser-hop1/exchange` with its projected ServiceAccount token.
4. Identity TokenReviews that workload caller, validates Steward's projected public JWKS,
   consumes the assertion identifier once, and returns a no-store, 60-second MCP bearer.
5. The broker uses that bearer only for its immediately adjacent private MCP-GW request. It is
   never returned by a Steward browser handler, put in a URL, serialized, or logged.

Identity's corresponding verifier contract is `docs/browser-hop1-contract-v1.md` in the Identity
source repository. The shared v1 assertion contains exactly `iss`, `sub`, `aud`, `exp`, `iat`,
`nbf`, `jti`, `email`, `email_verified`, `operation`, and `operation_id`.
`operation` is fixed to `github_oauth_connect`; its opaque `op_<32 lowercase hex>` identifier and
assertion `jti` are minted by Steward. Browser binding, RBAC roles, OAuth provider data, and any
credential are deliberately absent.

## Deployment inputs

All of these settings are required together; any partial configuration fails process startup.

| Variable | Meaning |
| --- | --- |
| `STEWARD_MCP_GW_ORIGIN` | Exact private MCP-GW origin; normal deployments require HTTPS. |
| `STEWARD_IDENTITY_BROWSER_HOP1_ENDPOINT` | Exact private HTTPS Identity endpoint, ending in `/v1/browser-hop1/exchange`. |
| `STEWARD_BROWSER_HOP1_ISSUER` | Exact HTTPS issuer in Steward's ES256 assertion. |
| `STEWARD_BROWSER_HOP1_ASSERTION_AUDIENCE` | Exact Identity-only assertion audience. |
| `STEWARD_BROWSER_HOP1_KEY_ID` | Active ES256 signing `kid`. |
| `STEWARD_BROWSER_HOP1_SIGNING_KEY_FILE` | Read-only PKCS#8 P-256 private-key projection. |
| `STEWARD_BROWSER_HOP1_JWKS_FILE` | Read-only public ES256 JWKS projection. |
| `STEWARD_BROWSER_HOP1_SERVICE_ACCOUNT_TOKEN_FILE` | Read-only projected ServiceAccount token for the Identity workload listener. |

The private signing key is a Secret projection, never chart values, source, browser state, or a
ConfigMap. The public JWKS is a separately versioned immutable ConfigMap projection. It must have
the active `kid`, `kty=EC`, `crv=P-256`, `use=sig`, and `alg=ES256`. Steward validates at startup
that a proof signed by the private key verifies with that exact JWKS key and derives the active
public JWK from the PKCS#8 signer; a mismatched, malformed, or incomplete pair fails closed before
any browser route is mounted. The deployment's public ConfigMap is therefore a non-secret release
artifact derived from that signer, not an independently chosen key.

Identity independently mounts the same public JWKS with a rollout revision and grants its
`browser-hop1-issuer` workload role only to the Steward API ServiceAccount. Rotation first
projects the overlapping public set, then selects the new Steward `kid`, then removes the retired
key only after all pre-existing assertions have expired. The short 60-second maximum assertion
lifetime bounds that overlap.

No public Ingress, callback, or browser JavaScript route is created by this contract. A platform
adapter is responsible only for projecting the Secret, public ConfigMap, ServiceAccount token,
and the existing private workload network path; it must not rewrite claims or introduce a
second signer.
