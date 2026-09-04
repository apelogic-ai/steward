# Steward runtime-provider profile bundle 1.3.0

This immutable bundle supersedes `steward-runtime-providers@1.2.0`. It retains the
two unversioned profiles byte-for-byte for legacy and governed Connections runtimes,
and adds two versioned Task-agent profiles:

- `steward-mcp-gw-v1-3-0`
- `steward-litellm-v1-3-0`

The versioned profiles authorize the native Codex 0.140 executable paths and are the
only profiles selected by a Task bound to
`nativeProfile=steward-runtime-providers@1.3.0`. Keeping the provider IDs versioned
lets queued Tasks continue to select their persisted native-policy generation rather
than whichever policy later occupies an unversioned name.

The bundle is deployment-neutral. A deployment supplies only the exact HTTPS origins
and canonical service CIDRs declared in `bundle.json`; it cannot supply credentials or
policy-bearing fields. The two versioned profiles must receive the same environment
bindings as their corresponding unversioned profiles.

An input document must bind to this exact identity and all four profiles:

```json
{
  "schema": "steward.provider-profile-inputs/v1",
  "bundle": {"id": "steward-runtime-providers", "version": "1.3.0"},
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
    },
    {
      "id": "steward-mcp-gw-v1-3-0",
      "inputs": {
        "gateway-origin": "https://mcp.gateway.test",
        "runtime-grant-origin": "https://mint.gateway.test",
        "service-cidrs": ["10.42.0.0/16"]
      }
    },
    {
      "id": "steward-litellm-v1-3-0",
      "inputs": {
        "gateway-origin": "https://inference.gateway.test",
        "runtime-grant-origin": "https://mint.gateway.test",
        "service-cidrs": ["10.42.0.0/16"]
      }
    }
  ]
}
```

## Upgrade from 1.2.0

Keep the original 1.2.0 input document and create a 1.3.0 input document with
identical environment values for each corresponding profile. Stop the consumer that
reconciles the rendered directory, then run:

```sh
cargo xtask provider-profile-bundle upgrade \
  --from-inputs inputs-1.2.0.json \
  --inputs inputs-1.3.0.json \
  --output rendered-profiles
```

The migration requires the current 1.2.0 installation to match its frozen source,
preserves both existing profiles exactly, verifies that the two new versioned profiles
have identical environment bindings, and permits only the declared Codex 0.140 binary
paths in the new profiles. It stages and validates the complete replacement before
swapping directories and restores the predecessor if publication fails.

Fresh installations use the exact-version path:

```sh
cargo xtask provider-profile-bundle install --inputs inputs-1.3.0.json --output rendered-profiles
cargo xtask provider-profile-bundle reconcile --inputs inputs-1.3.0.json --output rendered-profiles
```

## Verify a released bundle before rendering or installation

The release handoff is the authority for the immutable asset name, bundle identity,
pinned `sha256:<hex>` digest, signer identity, source repository and exact source commit.
Verify the downloaded archive before extraction:

```sh
gh attestation verify "$asset" \
  --repo "$repository" \
  --cert-identity "$signer_identity" \
  --format json > "${asset}.provenance.json"

actual_digest="sha256:$(sha256sum "$asset" | awk '{print $1}')"
test "$actual_digest" = "$expected_digest"
```

Confirm that verified provenance records the expected source repository and exact
source commit; source-only validation does not make this bundle eligible for release;
the release workflow also requires its native OpenShell and governed-runtime E2E lanes.
