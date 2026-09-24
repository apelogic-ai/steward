# Codex reference runtime

Steward releases one supported reference image for `codex@0.140.0` on `linux/amd64`. The release
handoff records its OCI reference and immutable digest under `referenceRuntimes.codex`. The image is
built from [`build/codex-reference.Dockerfile`](../../build/codex-reference.Dockerfile), with both
the OpenShell sandbox base and Codex CLI version pinned.

Use the digest from the release handoff, never the convenience tag, in an execution binding:

```yaml
agentRef: codex@0.140.0
adapter: codex-v1
image: ghcr.io/apelogic-ai/steward-codex-runtime@sha256:<release-digest>
executable: /usr/bin/codex
versionProbe:
  arguments: [--version]
  expectedStdout: codex-cli 0.140.0
```

Install the provider-profile bundle from the same Steward release and record the rendered profile
IDs and digests in the binding. The release gate executes
[`scripts/codex-reference-runtime-conformance.sh`](../../scripts/codex-reference-runtime-conformance.sh)
against the published image and that exact bundle. It verifies the platform, non-root runtime user,
CLI version, native executable, and both released profile allowlists.

## Mirror the exact manifest

Download and verify `release-handoff.json`, then copy the digest-selected manifest without rebuilding
it:

```sh
source_reference="$(jq -r '.referenceRuntimes.codex.reference' release-handoff.json)"
source_digest="$(jq -r '.referenceRuntimes.codex.digest' release-handoff.json)"
source_image="${source_reference}@${source_digest}"
target_image="registry.example.test/agents/steward-codex-runtime:0.140.0-steward-0.2.2"

docker buildx imagetools create --tag "${target_image}" "${source_image}"
docker buildx imagetools inspect "${source_image}"
docker buildx imagetools inspect "${target_image}"
```

Resolve the target registry's immutable digest after the copy and use
`registry.example.test/agents/steward-codex-runtime@sha256:<target-digest>` in the binding. Retain the
source coordinate, source digest, target coordinate, and target digest as deployment evidence.

## Build a compatible private runtime

Forks may build an equivalent image from the public definition instead of mirroring the released
manifest:

```sh
docker buildx build \
  --platform linux/amd64 \
  --file build/codex-reference.Dockerfile \
  --provenance=mode=max \
  --sbom=true \
  --tag registry.example.test/agents/codex:0.140.0 \
  --push .
```

Run the same conformance contract against the resulting immutable digest and the verified provider
bundle from a `linux/amd64` host with Docker and the pinned Kubernetes tools before activation:

```sh
scripts/codex-reference-runtime-conformance.sh \
  --image registry.example.test/agents/codex@sha256:<private-digest> \
  --provider-profile-bundle steward-runtime-providers-0.2.2.tar.gz
```

A private image that passes this contract is compatible with `codex-v1`; it is independently built
and operated. The released reference digest remains the reproducible Steward-tested artifact.
