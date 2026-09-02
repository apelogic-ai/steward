# Steward runtime-provider profile bundle 1.2.0

This immutable bundle supersedes `steward-runtime-providers@1.1.0`. It adds
only `/usr/local/bin/steward-connections-bridge` to the MCP provider profile so
the governed, one-shot provider-control runtime can obtain an OpenShell token
grant. The inference profile and every endpoint, authorization, capability,
and network-policy field remain unchanged.

The bundle is deployment-neutral. A deployment supplies only the exact HTTPS
origins and canonical service CIDRs declared in `bundle.json`; it cannot supply
credentials or policy-bearing fields.

An input document must bind to this exact identity:

```json
{
  "schema": "steward.provider-profile-inputs/v1",
  "bundle": {"id": "steward-runtime-providers", "version": "1.2.0"},
  "profiles": [
    {
      "id": "steward-mcp-gw",
      "inputs": {
        "gateway-origin": "https://mcp.gateway.test",
        "runtime-grant-origin": "https://mint.gateway.test",
        "service-cidrs": ["10.42.0.0/16"]
      }
    },
    {
      "id": "steward-litellm",
      "inputs": {
        "gateway-origin": "https://inference.gateway.test",
        "runtime-grant-origin": "https://mint.gateway.test",
        "service-cidrs": ["10.42.0.0/16"]
      }
    }
  ]
}
```

## Upgrade from 1.1.0

Keep the original 1.1.0 input document and create a 1.2.0 input document with
the same environment values. Stop the consumer that reconciles the rendered
directory, then run the explicit migration:

```sh
cargo xtask provider-profile-bundle upgrade \
  --from-inputs inputs-1.1.0.json \
  --inputs inputs-1.2.0.json \
  --output rendered-profiles
```

The command first renders 1.1.0 from its frozen source and requires the current
installation to match it exactly. It rejects drift, changed environment
bindings, any unknown predecessor, any destination other than the declared
1.1.0 to 1.2.0 transition, and any profile change except the single governed
bridge binary addition to the MCP profile. It stages and validates the complete
replacement before swapping directories, restores the predecessor if
publication fails, and verifies 1.2.0 after the swap. Restart the consumer only
after the command succeeds.

Fresh installations use the ordinary exact-version path:

```sh
cargo xtask provider-profile-bundle install --inputs inputs-1.2.0.json --output rendered-profiles
cargo xtask provider-profile-bundle reconcile --inputs inputs-1.2.0.json --output rendered-profiles
```

## Verify a released bundle before rendering or installation

The release handoff is the authority for the immutable asset name, bundle
identity, pinned `sha256:<hex>` digest, signer identity, source repository, and
source commit. Do not render or install an archive based only on its tag or
filename. A consumer must perform this sequence for the exact release values
in that handoff:

```sh
repository=apelogic-ai/steward
tag=vX.Y.Z
asset=steward-runtime-providers-X.Y.Z.tar.gz
expected_digest=sha256:... # copied from the release handoff
signer_identity="https://github.com/${repository}/.github/workflows/release.yml@refs/tags/${tag}"
source_commit=... # copied from the release handoff

gh release download "$tag" --repo "$repository" --pattern "$asset"
gh attestation verify "$asset" \
  --repo "$repository" \
  --cert-identity "$signer_identity" \
  --format json > "${asset}.provenance.json"

# Confirm the verified provenance records the handoff's source repository and
# exact source commit before trusting the archive bytes.
jq -e --arg repository "https://github.com/${repository}" \
  --arg commit "$source_commit" '
  tostring | contains($repository) and contains($commit)
' "${asset}.provenance.json"

actual_digest="sha256:$(sha256sum "$asset" | awk '{print $1}')"
test "$actual_digest" = "$expected_digest"
```

Only after all checks succeed may the consumer extract the archive, confirm
that `bundle.json` identifies `steward-runtime-providers@1.2.0`, validate the
manifest and templates, and run the product-owned renderer, installer, or
upgrade command. The source-only validation does not make this bundle eligible for release.
The release workflow also requires the authenticated OpenShell adapter and governed
Connections E2E lanes on their native linux/amd64 runners.
