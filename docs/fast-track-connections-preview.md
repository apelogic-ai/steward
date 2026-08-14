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

Network policy must allow only the Steward apiserver pod selector to reach bridge port 18080. The
governed sandbox profile must allow the bridge binary to reach only the configured MCP-GW
origin/port and three OAuth paths (plus DNS). Do not reuse the Codex/curl-only AgentGateway profile:
the bridge performs direct bounded HTTP to the unchanged MCP-GW OAuth routes.
