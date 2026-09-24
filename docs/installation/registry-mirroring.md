# Registry mirroring and deployment locks

Each Steward release attaches `steward-registry-lock.py` and
`steward-registry-lock.py.sha256`. The tool copies every component listed in the
verified `release-handoff.json`, including Steward images and reference coding-agent
runtimes. It then resolves each target-registry digest, inspects the exact digest, and
writes a deterministic `steward.deployment-lock/v1` document.

The tool requires Python 3 and Docker with the Buildx imagetools plugin. It accepts no
registry credential flags. Authenticate source and target registries beforehand with
the standard `docker login` flow or a Docker credential helper, keep shell tracing
disabled, and never place a password in an argument or lock file.

## Verify and run

Download `release-handoff.json`, the tool, and its checksum from the same release. First
verify the handoff attestation as described in the release notes, then verify the tool:

```sh
sha256sum --check steward-registry-lock.py.sha256

set +x
docker login registry.example.test
python3 steward-registry-lock.py mirror \
  --handoff release-handoff.json \
  --target-prefix registry.example.test/team-a \
  --output steward-deployment-lock.json
```

The default mode requires each source artifact to be an OCI index. The target must also
be an index with the exact source descriptor set. This preserves all released platforms
and associated index descriptors where the target registry supports them. The target
index digest is resolved after the copy; the source digest is never reused as an
assumption.

Some registries or deployment lanes intentionally carry one platform. Make that
narrowing explicit and give the target tags a distinguishing suffix:

```sh
python3 steward-registry-lock.py mirror \
  --handoff release-handoff.json \
  --target-prefix registry.example.test/team-a \
  --platform linux/amd64 \
  --target-tag-suffix=-amd64 \
  --output steward-deployment-lock-linux-amd64.json
```

Single-platform mode selects exactly one descriptor from every source index, copies it
without wrapping it in another index, inspects the exact target digest, and records both
the original index digest and selected platform digest. A single manifest is rejected in
default mode so it cannot silently masquerade as the released index.

## Lock contract

The lock contains no registry credentials. Object keys and arrays have deterministic
ordering, and identical verified inputs produce identical bytes. A shortened example is:

```json
{
  "artifacts": {
    "images.apiserver": {
      "copyMode": "index",
      "platforms": [
        {
          "architecture": "amd64",
          "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
          "os": "linux"
        }
      ],
      "source": {
        "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "reference": "ghcr.io/example-org/steward:0.2.2-apiserver"
      },
      "target": {
        "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "reference": "registry.example.test/team-a/steward:0.2.2-apiserver"
      }
    }
  },
  "chartValues": {
    "images": {
      "apiserver": {
        "digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "tag": "0.2.2-apiserver"
      },
      "repository": "registry.example.test/team-a/steward"
    }
  },
  "executionBindingImages": {
    "codex": "registry.example.test/team-a/steward-codex@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
  },
  "mode": "index",
  "release": {
    "commit": "0123456789abcdef0123456789abcdef01234567",
    "version": "0.2.2"
  },
  "requestedPlatform": null,
  "schemaVersion": "steward.deployment-lock/v1"
}
```

Copy `chartValues` into the installation values overlay. The optional bridge entry is
emitted as `chartValues.connectionsBridge.image`. Copy the desired value from
`executionBindingImages` into the matching execution binding's `image` field. Keep
deployment values digest-pinned; tags remain labels and are never the authority for a
deployment.
