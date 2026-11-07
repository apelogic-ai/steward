# Actions Runner Controller dependency chart

This Steward release chart wraps the pinned
`gha-runner-scale-set-controller` `0.15.0` chart. It installs the controller and
runner CRDs; a runner scale set and its registration Secret remain separate,
operator-owned inputs.

## Values mapping

| Purpose | Upstream value |
|---|---|
| Controller image | `arc.image.repository`, `arc.image.tag` |
| Controller namespace | `arc.namespaceOverride` |

The image tag must include an `@sha256:` digest. The chart creates no
Namespace, accepts no inline registration credential, and disables free-form
controller environment inputs.

## Platform requirements

Create `arc.namespaceOverride` before installing. The upstream chart creates
runner CRDs, ClusterRoles, and ClusterRoleBindings. A later runner scale-set
install needs its own namespace and Pod Security policy suited to the selected
runner container mode; do not place unrelated workloads there.
