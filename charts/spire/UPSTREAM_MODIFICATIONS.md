# Upstream modifications

The CRDs in `crds/` are rendered from the pinned `spire-crds` 0.5.0 chart
with its default annotations and moved to Helm's pre-install CRD directory.
Their Kubernetes schemas are unchanged. Chart preparation verifies the
upstream archive checksum and byte-compares every rendered CRD.
