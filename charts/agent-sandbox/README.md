# agent-sandbox dependency chart

This chart wraps the upstream `helm/` chart from the pinned `v0.5.0` source
archive. Preparation verifies the checksum recorded in `Chart.yaml` and copies
the upstream chart without copying or patching its workload resources. It
adjusts the shared label helper to remove the digest only from the chart's
version label, while retaining it in the image reference, as recorded in
`UPSTREAM_MODIFICATIONS.md`.

## Values mapping

| Purpose | Value |
|---|---|
| Controller image | `agentSandbox.image.repository`, `agentSandbox.image.tag` |
| Controller and webhook namespace | `agentSandbox.namespace.name` |

## Platform requirements

Create `agentSandbox.namespace.name` before installing. The chart locks
`agentSandbox.namespace.create` to `false`, so it creates no Namespace. It
installs the upstream Sandbox and extension CRDs, ClusterRoles,
ClusterRoleBindings, conversion webhook Services, and controller. The upstream
ClusterRoles use CRD-wide verbs for sandbox and extension reconciliation.

Sandbox workloads may need a RuntimeClass, privileged node components, host
mounts, and capabilities selected by the runtime implementation. Keep their
namespace separate from the controller and unrelated workloads, and apply the
minimum Pod Security exception required by that runtime.
