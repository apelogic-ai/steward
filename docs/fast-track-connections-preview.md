# Fast-Track Connections preview

Status: **FAST-TRACK / NON-PROMOTABLE**. This is a DEV-only hand-test artifact. It has no path to
merge, release, promotion, or production without replacing this seam and repeating normal gates.

The preview is two processes:

1. `fast-track-steward-preview` serves real Google organization sign-in and the Connections BFF.
2. `fast-track-connections-bridge` runs inside the fixed governed OpenShell sandbox. It accepts
   only the three internal status/start/disconnect operations, checks one fixed issuer/email pair,
   and sends only `Authorization: Bearer openshell-token-grant-placeholder` to the exact configured
   MCP-GW `/oauth/github/status`, `/oauth/github/start`, and `/oauth/github/disconnect` routes.

Neither process accepts a HOP-1 token setting. The bridge has no token-file option. OpenShell owns
transparent substitution at the allowed sandbox egress. MCP-GW remains the only holder of GitHub
provider credentials.

Before the first connection request, the preview may create the current DEV controller's legacy
email-bound runtime through `POST /admin/api/v1/fast-track/connections/runtime`. This endpoint is
behind the normal Google browser-session, same-origin, JSON, and CSRF boundary. It accepts exactly
`{}` and derives the acting user and owner only from the verified browser principal. The fixed
runtime is `lbe259-fast-track/connections-bridge`, has no `canonicalAuthority`, uses the fixed
service principal `steward-run` with the verified email as `actingUser`, has a 15-minute TTL and
zero model/budget allocation, and grants only
`github/github_oauth_start/write`. A second call from the same browser session is idempotent; a
different session or a mismatched pre-existing runtime fails closed. Its response contains only
the fixed runtime identifier and sanitized phase. This is a temporary Mint v2/DEV CRD
compatibility seam, not the production identity architecture.

Both processes are bounded by `STEWARD_FAST_TRACK_BRIDGE_TTL_SECONDS`, which must be between 1 and
3600. The fixed browser email and canonical ID must describe the same currently verified user as
the fixed issuer/email authority configured for the governed bridge sandbox.

## Build inputs

```bash
docker build -f build/fast-track-preview.Dockerfile \
  --build-arg EXAMPLE=fast-track-connections-bridge \
  -t steward-connections-bridge:fast-track .

docker build -f build/fast-track-preview.Dockerfile \
  --build-arg EXAMPLE=fast-track-steward-preview \
  -t steward-preview:fast-track .
```

The bridge listens on the explicit `STEWARD_FAST_TRACK_BRIDGE_BIND` value. Use `0.0.0.0:18080` in
the sandbox and expose only port 18080 through an internal ClusterIP. `/healthz` returns 204 while
the TTL is active and 410 after expiry.

Bridge configuration is non-secret:

- `STEWARD_FAST_TRACK_MCP_GW_ORIGIN`: exact wrapper origin only; no path, query, or userinfo.
- `STEWARD_FAST_TRACK_COMPATIBILITY_ISSUER`: exact HTTPS Steward Mint compatibility issuer.
- `STEWARD_FAST_TRACK_COMPATIBILITY_EMAIL`: current verified email for the single preview user.
- `STEWARD_FAST_TRACK_REDIRECT_AFTER`: exact HTTPS `/admin/connections` URL, optionally with the
  `github-connected` fragment.
- `STEWARD_FAST_TRACK_BRIDGE_TTL_SECONDS`: 1–3600.

The Steward preview additionally requires:

- `STEWARD_FAST_TRACK_PREVIEW_BIND` (use `0.0.0.0:8080` behind the existing DEV TLS ingress);
- `STEWARD_FAST_TRACK_BRIDGE_ORIGIN` (the bridge's internal ClusterIP service origin);
- `STEWARD_FAST_TRACK_CANONICAL_USER_ID` (the existing internal canonical ID for that user);
- `STEWARD_BROWSER_ORIGIN`, `STEWARD_ORGANIZATION_ID`, `STEWARD_GOOGLE_HOSTED_DOMAIN`, and
  `STEWARD_GOOGLE_CLIENT_ID`;
- `STEWARD_GOOGLE_CLIENT_SECRET`, projected only into the preview process through the reviewed
  existing secret path. It is never projected into the bridge sandbox.

The preview ServiceAccount additionally needs only namespaced `get` and `create` for
`agentruntimes.agents.apelogic.ai` in `lbe259-fast-track`, and its exact Kubernetes username must
already be configured as a trusted Steward writer. Runtime creation uses that ServiceAccount
authority directly; it does not impersonate the browser user or any group. The live `steward-run`
service envelope must admit exactly the fixed tool, zero budget, and 15-minute TTL. The preview
does not need list/watch/update/patch/delete, impersonation, or Secret access for this endpoint.

Network policy must allow only the Steward apiserver pod selector to reach bridge port 18080. The
governed sandbox profile must allow the bridge binary to reach only the configured MCP-GW
origin/port and three OAuth paths (plus DNS). Do not reuse the Codex/curl-only AgentGateway profile:
the bridge performs direct bounded HTTP to the unchanged MCP-GW OAuth routes.
