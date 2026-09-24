# Governed-platform compatibility

Status: **Current release contract for Steward v0.2.3**

The release asset `steward-governed-platform-compatibility-0.2.3.json` is the
machine-readable source of truth for the exact combination tested with this
Steward release. It covers Steward, `steward-run`,
`github-oidc-exchange`, OpenShell, agent-sandbox, SPIRE, MCP-GW, and LiteLLM.
Use its immutable references rather than reconstructing versions from prose.

## Acquire and verify

Download the compatibility manifest, checksum, release handoff, and attestation
from the same GitHub release:

```sh
gh release download v0.2.3 \
  --repo apelogic-ai/steward \
  --pattern steward-governed-platform-compatibility-0.2.3.json \
  --pattern steward-governed-platform-compatibility-0.2.3.json.sha256 \
  --pattern release-handoff.json

sha256sum --check steward-governed-platform-compatibility-0.2.3.json.sha256
gh attestation verify steward-governed-platform-compatibility-0.2.3.json \
  --repo apelogic-ai/steward \
  --cert-identity https://github.com/apelogic-ai/steward/.github/workflows/release.yml@refs/tags/v0.2.3
```

Also compare the calculated digest with
`governedPlatformCompatibility.digest` in `release-handoff.json`. A fork uses
its own repository, tag, signer identity, and released coordinates.

## SPIRE contract

The compatibility manifest names both required charts and every image observed
in the tested render. Fetch the exact chart versions from
`https://spiffe.github.io/helm-charts-hardened/` and verify the downloaded chart
archive SHA-256 before installation. Install or upgrade `spire-crds` before
`spire`, verify the SPIRE server, agents, CSI driver, and controller manager,
then verify Steward's `ClusterSPIFFEID` before rolling Mint.

The installation trust domain is operator-supplied and immutable. Mint's
identity is:

```text
spiffe://<trust-domain>/steward/mint
```

The Steward chart owns the `ClusterSPIFFEID` that selects the Mint pod in the
release namespace. Its trust domain must agree with SPIRE, the Mint
configuration, and workload verification. Changing the trust domain is an
identity migration, not an in-place chart value edit.

## Inference contract

The tested LiteLLM release and immutable image are in the compatibility
manifest. Runtime-scoped keys remain the inference authority. Agent adapters
apply different URL conventions deliberately:

| Adapter | Configured URL | Required operation | Model identifier |
|---|---|---|---|
| `codex-v1` | Exact operation URL | `/v1/responses` | Provider-qualified; tested as `openai/gpt-5.4` |
| `claude-code-v1` | API base URL | `/v1/messages` appended by the client | Provider-qualified; tested as `anthropic/claude-sonnet-4-6` |

Do not append `/v1/responses` twice for Codex and do not configure Claude with
an already-expanded Messages operation URL. Before enabling a binding, prove
the corresponding real HTTP operation through the selected LiteLLM deployment
with the exact admitted model ID. A healthy management endpoint alone is not
inference conformance.

## MCP-GW authority contract

The public chart selector is an authority contract, not an MCP-GW product
version:

| Selector | Compatible MCP-GW releases | Meaning |
|---|---|---|
| `steward.connections.github/v1` | `0.3.2` | Original GitHub connection authority response |
| `steward.connections.github/v2` | `0.4.9`, `0.4.10`, `0.4.11` | Additive lifecycle status and renewal metadata; unknown additive fields are accepted |

Set `connectionsBridge.mcpGatewayAuthorityContract` to the contract declared by
the compatibility manifest. The deprecated `mcpGatewayVersion` value remains
only for existing installations: use either the authority-contract selector or
the legacy value, never both. Migrate by replacing legacy `0.3.2` with `v1`, or
legacy `0.4.9` with `v2`; this changes selection vocabulary, not runtime
authority.

## Gateway API backend TLS contract

Gateway API `v1.4.0` or later is required because it carries the GA
`gateway.networking.k8s.io/v1` `BackendTLSPolicy`. The selected
`GatewayClass.status.supportedFeatures` must include `BackendTLSPolicy`; it is
an extended feature, so presence of the CRD alone is not sufficient. Envoy
Gateway `v1.3.0` or later is the documented minimum for Steward's named
Service-port attachment.

When `web.httpRoute.enabled=true`, the chart creates a `BackendTLSPolicy` in
the Steward release namespace. It targets `steward-apiserver` section `https`,
uses SNI
`steward-apiserver.<release-namespace>.svc.<cluster-domain>`, and refers only
to a same-namespace public CA ConfigMap at `data.ca.crt`. The apiserver
certificate contains that DNS name. The policy never references or exposes the
TLS private key. Configure the CA ConfigMap as the output of a trust-distribution
controller so issuer rotation updates trust without manually editing the policy.

The Gateway API and controller are platform dependencies: this chart neither
installs nor upgrades them. Validate the resource, GatewayClass feature, policy
conditions, endpoints, certificate/SNI, and the public session route as
described in the [Gateway backend TLS procedure](platform-preflight.md#gateway-backend-tls).

## Deployment acceptance

Before governed execution, verify the exact companion/dependency coordinates,
SPIRE identity, both configured inference operation shapes, the MCP-GW
authority contract, and the selected OpenShell/agent-sandbox runtime. The
[platform preflight](platform-preflight.md) validates composed non-secret
configuration; the compatibility manifest defines which product combination
that configuration is expected to represent.
