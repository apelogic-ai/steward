# agent-sandbox dependency chart

The upstream `v0.5.0` release has no Helm chart. Preparation downloads its
published `manifest.yaml`, verifies the checksum recorded in `Chart.yaml`,
moves the CRD into `crds/`, removes the Namespace, and applies only the value
substitutions needed for the controller image, resource prefix, and target
namespace. The released chart therefore contains the pinned upstream manifest,
not a separately maintained workload copy.

## Values mapping

| Purpose | Value |
|---|---|
| Controller image | `image.repository`, `image.tag`, `image.digest` |
| Controller and webhook namespace | `targetNamespace` |
| Resource prefix | Helm release name or `fullnameOverride` |
| CRD namespace-patch image | `crdNamespacePatch.image.*` |

## Platform requirements

Create `targetNamespace` before installing. The chart creates no Namespace.
It creates the Sandbox CRD, ClusterRole, ClusterRoleBinding, conversion webhook
Service, and controller. A small pre-install hook updates the CRD webhook
reference to the configured resource name and namespace; the hook can get and
patch only that CRD.

Sandbox workloads may need a RuntimeClass, privileged node components, host
mounts, and capabilities selected by the runtime implementation. Keep their
namespace separate from the controller and unrelated workloads, and apply the
minimum Pod Security exception required by that runtime.
