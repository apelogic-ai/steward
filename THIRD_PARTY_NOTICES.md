# Third-party notices

Steward's own source and chart are licensed under the root [MIT license](LICENSE).
The following checked-in patch material retains its upstream license and
copyright; the Steward MIT notice does not replace those terms.

| Material | Pinned upstream source | License copy |
|---|---|---|
| MCP-GW verified-claims patch | [apelogic-ai/mcp-gw `c2af10d9`](https://github.com/apelogic-ai/mcp-gw/tree/c2af10d9c3dee898e368e6cf3d0f5a1ef6ad0dde) | [MIT, Copyright (c) 2026 ApeLogic AI](third_party/mcp-gw-patches/c2af10d9/LICENSE) |
| agent-sandbox chart manifest | [kubernetes-sigs/agent-sandbox `v0.5.0`](https://github.com/kubernetes-sigs/agent-sandbox/releases/tag/v0.5.0) | Apache-2.0; the release manifest is verified by its published SHA-256 before packaging |
| OpenShell chart dependency | [NVIDIA/OpenShell `v0.0.98`](https://github.com/NVIDIA/OpenShell/releases/tag/v0.0.98) | Apache-2.0; packaged from the pinned upstream OCI chart |
| SPIRE chart dependencies | [spiffe/helm-charts-hardened `spire-0.29.0` and `spire-crds-0.5.0`](https://github.com/spiffe/helm-charts-hardened) | Apache-2.0; packaged from the pinned upstream chart repository |
| LiteLLM chart dependency | [BerriAI/litellm `v1.106.2`](https://github.com/BerriAI/litellm/releases/tag/v1.106.2) | MIT; the source archive is verified by SHA-256 before packaging |
| Actions Runner Controller chart dependency | [actions/actions-runner-controller `gha-runner-scale-set-0.15.0`](https://github.com/actions/actions-runner-controller/releases/tag/gha-runner-scale-set-0.15.0) | Apache-2.0; packaged from the pinned upstream OCI chart |

The patch README records the exact base, purpose, and removal condition.
Redistributed Rust crates, JavaScript packages, Helm dependencies, base images,
and operating system packages retain their own licenses. The release workflow
produces an SPDX software bill of materials for each image and chart; review that
artifact's exact dependency set and carry any required upstream license and
notice texts when redistributing it. This source notice is not a substitute
for the artifact-specific dependency license bundle.
