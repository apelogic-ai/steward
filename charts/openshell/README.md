# OpenShell dependency chart

This Steward release chart wraps the pinned OpenShell `0.0.98` chart without
copying its workload templates. It is an optional standalone install and a
dependency of `steward-bundle`; the existing `charts/steward` chart does not
depend on it.

## Values mapping

| Purpose | Upstream value |
|---|---|
| Gateway image | `openshell.image.repository`, `openshell.image.tag` |
| Supervisor image | `openshell.supervisor.image.repository`, `openshell.supervisor.image.tag` |
| Default sandbox image | `openshell.server.sandboxImage` |
| Existing Secret | `openshell.server.externalDbSecret`, `openshell.server.tls.*`, `openshell.server.sandboxJwt.signingSecretName` |
| Shared Gateway route | `route.parentRefs`, `route.hostname`, `route.timeouts.*` |

Tags without an `@sha256:` digest are refused. Every Secret reference must
name the same existing Secret. Its fixed upstream keys are `uri`, `tls.crt`,
`tls.key`, `ca.crt`, `signing.pem`, `public.pem`, and `kid`. The chart never
renders Secret data.

## Platform requirements

Create the release namespace and `openshell.server.sandboxNamespace` before
installing. The chart creates no Namespace. OpenShell creates cluster-scoped
RBAC used to manage sandbox workloads. The sandbox namespace must allow the
privileged runtime and Linux capabilities required by the selected OpenShell
profile; keep unrelated workloads out of that namespace.

The route is off by default. Enabling it requires an existing Gateway, an
exact hostname, and request plus backend-request timeouts.
