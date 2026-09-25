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
CLI version, native executable, both released profile allowlists, and startup with both profiles on
OpenShell's sidecar supervisor topology with binary-aware network policy enabled. That topology is
part of the tested deployment contract. Pull-request CI runs the same live conformance against the
locally built image and provider bundle before the release workflow repeats it against published
digests.

## Mirror the exact manifest

Use the released [daemonless registry mirror](registry-mirroring.md). Add an
explicit `referenceRuntimes.codex` target to the mapping, review the no-write
plan, and run `mirror`. The resulting deployment lock supplies the exact
digest-pinned value at `executionBindingImages.codex`; use that value in the
binding. The lock retains the source and target coordinates and digests as
deployment evidence.

For an existing Docker-only mirror automation, the equivalent manifest copy is
`docker buildx imagetools create --tag <target> <source@digest>`. It must copy
the source digest without rebuilding it; use the resulting target digest in
the binding. New installations should use the released ORAS-based mirror so a
Docker daemon is not required.

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
  --provider-profile-bundle steward-runtime-providers-0.2.5.tar.gz
```

A private image that passes this contract is compatible with `codex-v1`; it is independently built
and operated. The released reference digest remains the reproducible Steward-tested artifact.
