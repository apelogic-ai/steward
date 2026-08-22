# Steward runtime-provider profile bundle v1

This directory is a **product-owned contract** for the portable
part of the OpenShell provider-profile configuration. It replaces the former
model in which a deployment repository copied and rewrote profile semantics.

`bundle.json` names the version, profile IDs, template files, and the only
safe state transitions. The profiles define the capabilities Steward requests,
their required HTTPS transport, and the binaries OpenShell must permit.

The bundle intentionally contains no deployment value. In particular it must
not contain cluster DNS, an endpoint URL, a provider token, a secret reference,
an OAuth setting, a certificate, or a CA setting. A deployment adapter may
provide only these named inputs at render time:

- `gateway-origin` (`https-origin`): the target provider's HTTPS origin.
- `runtime-grant-origin` (`https-origin`): Steward's runtime token-grant HTTPS
  origin. It contains no token, key, secret reference, certificate, CA, or
  OAuth client configuration.
- `service-cidrs` (`cidr-list`): the narrow network ranges that reach it.

The bundle owns the non-secret runtime-grant protocol (audience, scope, token
path, and bounded cache TTL). The adapter supplies only the concrete HTTPS
runtime-grant origin. It never supplies a bearer token, private key, secret
reference, certificate, CA, OAuth setting, profile ID, capability, or policy
field, and it must not copy, rewrite, or fork these files.

The input document has this exact schema; the profile IDs and input keys must
match the release bundle exactly:

```json
{
  "schema": "steward.provider-profile-inputs/v1",
  "bundle": {"id": "steward-runtime-providers", "version": "1.0.0"},
  "profiles": [{
    "id": "steward-mcp-gw",
    "inputs": {
      "gateway-origin": "https://mcp.gateway.test",
      "runtime-grant-origin": "https://mint.gateway.test",
      "service-cidrs": ["10.42.0.0/16"]
    }
  }]
}
```

Use the product-owned lifecycle commands rather than deployment scripts:

```sh
cargo xtask provider-profile-bundle install --inputs inputs.json --output rendered-profiles
cargo xtask provider-profile-bundle reconcile --inputs inputs.json --output rendered-profiles
```

Consumers must validate the checked-in release contract before packaging:

```sh
cargo xtask provider-profile-bundle validate
```

The validation is fail-closed: every profile template must be declared by the
manifest, every manifest template must be supplied, a template can reference
only named portable inputs, and non-portable configuration is rejected. The
product-owned renderer consumes an exact, bundle-bound input document and
produces OpenShell import profiles plus an installation state record. It will
not accept a partial profile set, an undeclared input, a non-HTTPS origin, a
non-canonical CIDR, or a different bundle version. Its installer permits only
an absent directory; its reconciler accepts only byte-for-byte same-bundle
state. Neither copies nor rewrites the checked-in templates.

## Verify a released bundle before rendering or installation

The release handoff is the authority for the immutable asset name, its pinned
`sha256:<hex>` digest, signer identity, source repository, and source commit.
Do not render or install an archive based only on its tag or filename. A
consumer must perform this sequence for the exact release values in that
handoff:

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

Only after all of those checks succeed may the consumer extract the archive,
validate the bundled manifest/templates, and run the product-owned renderer or
installer. The release workflow also requires the authenticated OpenShell
adapter E2E lane on its native linux/amd64 runner; source-only validation does
not make this bundle eligible for release.
