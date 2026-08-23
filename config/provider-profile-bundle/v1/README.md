# Steward runtime-provider profile bundle v1

This directory is a **product-owned, releaseable contract** for the portable
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
- `service-cidrs` (`cidr-list`): the narrow network ranges that reach it.

The adapter is also responsible for connecting the profile to Steward's normal
runtime identity and mint configuration. That binding is not part of a provider
profile template, and an adapter must not copy, rewrite, or fork these files to
perform it.

Consumers must validate the checked-in release contract before packaging:

```sh
cargo xtask provider-profile-bundle validate
```

The validation is fail-closed: every profile template must be declared by the
manifest, every manifest template must be supplied, a template can reference
only named portable inputs, and non-portable configuration is rejected. The
next slice supplies a signed release artifact and a product-owned renderer that
turns this contract plus environment inputs into an OpenShell profile without
changing the contract itself.
