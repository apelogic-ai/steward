# SPIRE dependency chart

This Steward release chart wraps `spire` `0.29.0` and `spire-crds` `0.5.0`.
It enables the server, agent, CSI driver, controller manager, and the
`ClusterSPIFFEID` needed by OpenShell sandboxes. Optional OIDC and UI workloads
remain disabled.

## Values mapping

| Purpose | Upstream value |
|---|---|
| SPIRE server image | `spire.spire-server.image.tag` |
| SPIRE agent image | `spire.spire-agent.image.tag` |
| Controller image | `spire.spire-server.controllerManager.image.tag` |
| CSI and registrar images | `spire.spiffe-csi-driver.image.tag`, `spire.spiffe-csi-driver.nodeDriverRegistrar.image.tag` |
| System and server namespaces | `spire.global.spire.namespaces.system.name`, `spire.global.spire.namespaces.server.name` |
| Trust domain and cluster | `spire.global.spire.trustDomain`, `spire.global.spire.clusterName` |

Every image tag includes an `@sha256:` digest. The wrapper disables upstream
namespace creation and lifecycle hooks so no unlisted helper image or Namespace
is introduced.

## Platform requirements

Create both configured namespaces before installing. The chart creates SPIRE
CRDs, CSIDriver, ClusterRoles, ClusterRoleBindings, and ClusterSPIFFEIDs. SPIRE
agents and the CSI driver need privileged host mounts and access to kubelet
paths; dedicate appropriately labelled nodes and apply a Pod Security policy
that permits only these workloads. The pinned CRD chart keeps its upstream
layout until the empty-cluster `steward-bundle` proof confirms one-release
installation ordering.
