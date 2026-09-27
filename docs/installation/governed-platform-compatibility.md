# Product compatibility and installation BOM

Status: **Current release contract for Steward v0.3.0**

Steward publishes a product-compatibility contract. Release/integration
packaging publishes the installation bill of materials (BOM). These are
separate authorities:

| Contract | Owner | Contents |
|---|---|---|
| Product compatibility | Steward | Supported Task API contract, dependency contracts, and minimum client capabilities |
| Installation BOM | Release/integration packaging | Exact Steward, `steward-run`, Identity, dependency, chart, image, and source-commit coordinates tested together |

The Steward source tree does not pin future companion-product artifacts. An
installation BOM is assembled only after every independently versioned product
release exists. GitOps and operators consume that signed, immutable BOM rather
than treating the Steward repository as a cross-product deployment lock.

## Product compatibility

The Steward v0.3.0 release includes the attested asset
`steward-product-compatibility-0.3.0.json`. Its source is
`config/product-compatibility/v1/compatibility.json` and its schema identity is
`steward.product-compatibility/v1`.

For v0.3.0 it declares:

- Task API contract `steward.task/v2`;
- the `envelopeDigest` selector requires `steward-run` v0.7.0 or later;
- Identity policy contract `github-oidc-exchange.apelogic.io/v5` and Task
  identity contract `steward-task-v2`;
- GitHub connection authority `steward.connections.github/v2`;
- Gateway API `v1` with the `BackendTLSPolicy` feature; and
- the exact-operation and base-URL semantics for the Codex and Claude inference
  adapters.

Download and verify it from the Steward release:

```sh
gh release download v0.3.0 \
  --repo apelogic-ai/steward \
  --pattern steward-product-compatibility-0.3.0.json \
  --pattern steward-product-compatibility-0.3.0.json.sha256 \
  --pattern release-handoff.json

sha256sum --check steward-product-compatibility-0.3.0.json.sha256
gh attestation verify steward-product-compatibility-0.3.0.json \
  --repo apelogic-ai/steward \
  --cert-identity https://github.com/apelogic-ai/steward/.github/workflows/release.yml@refs/tags/v0.3.0
```

Compare the calculated digest with `productCompatibility.digest` in
`release-handoff.json`. A fork uses its own repository, tag, signer identity,
and released coordinates.

## Installation BOM

The installation BOM is not a Steward source file or Steward release asset. It
is produced after Steward, `steward-run`, Identity, and other required products
have published their immutable release handoffs. At minimum, it records:

```json
{
  "steward": {
    "version": "0.3.0",
    "image": "registry.example.com/steward@sha256:<digest>"
  },
  "stewardRun": {
    "version": "0.7.0",
    "commit": "<40-hex-commit>",
    "workflowRepository": "example-org/steward-run",
    "workflowCommit": "<40-hex-commit>",
    "actionCommit": "<40-hex-commit>",
    "image": "registry.example.com/steward-run@sha256:<digest>",
    "chart": "registry.example.com/charts/steward-run-arc@sha256:<digest>"
  }
}
```

The real BOM also names every Steward component image, its chart, Identity, and
the external dependency coordinates used by the tested installation. Packaging
must verify each product handoff and the Steward product-compatibility contract
before signing the BOM. It must reject a `steward-run` release below v0.7.0 when
the installation uses `envelopeDigest`.

For Steward browser workflow generation, project the verified steward-run
release handoff into `config.apiserver.stewardRunRelease`. The field mapping is
exact:

| Verified steward-run handoff | Steward chart value |
|---|---|
| `schemaVersion` | `manifestSchemaVersion` |
| `version` | `version` |
| `workflowRepository` | `workflowRepository` |
| `workflowCommit` | `workflowCommit` |
| `actionCommit` | `actionCommit` |
| `image` | `governedJobContainerImage` |

For the v0.7.0 handoff, `schemaVersion` is the JSON number `3`. This normalized
projection renames fields but must not select different coordinates from the
signed BOM.

## Stable runtime contracts

The installation BOM chooses exact implementations; the following behavioral
contracts remain owned by Steward.

### SPIRE identity

The installation trust domain is operator-supplied and immutable. Mint uses
`spiffe://<trust-domain>/steward/mint`. The Steward chart owns the
`ClusterSPIFFEID` selecting Mint in the release namespace. Changing the trust
domain is an identity migration, not an in-place value edit.

### Inference

| Adapter | Configured URL | Required operation | Model identifier |
|---|---|---|---|
| `codex-v1` | Exact operation URL | `/v1/responses` | Provider-qualified |
| `claude-code-v1` | API base URL | `/v1/messages` appended by the client | Provider-qualified |

`config.controller.litellmUrl` is the LiteLLM management API base URL. Runtime
keys remain the inference authority. The target installation must prove the
real HTTP operation for each admitted model.

### MCP-GW authority

`connectionsBridge.mcpGatewayAuthorityContract` selects an authority contract,
not a product version. Steward v0.3.0 uses
`steward.connections.github/v2`. Release/integration packaging chooses and
records an MCP-GW release that implements that contract.

### Gateway API backend TLS

The selected `GatewayClass.status.supportedFeatures` must include
`BackendTLSPolicy`; CRD presence alone is insufficient. When
`web.httpRoute.enabled=true`, Steward creates a same-namespace
`BackendTLSPolicy` targeting the apiserver HTTPS port, with its public CA read
from `data.ca.crt` in an operator-owned ConfigMap. The chart never installs the
Gateway implementation or exposes the TLS private key.

Before governed execution, verify the signed installation BOM, the Steward
product contract, SPIRE identity, inference operation shapes, MCP-GW authority,
and the selected OpenShell/agent-sandbox runtime. The
[platform preflight](platform-preflight.md) validates composed non-secret
configuration; it does not select cross-product versions.
