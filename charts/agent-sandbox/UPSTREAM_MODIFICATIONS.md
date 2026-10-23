# Upstream modifications

The pinned controller image uses `tag@sha256` so the runtime pull is immutable.
Preparation strips the digest only from the upstream
`app.kubernetes.io/version` label because Kubernetes labels cannot contain `@`
or exceed 63 characters. The Deployment image retains the full digest.
