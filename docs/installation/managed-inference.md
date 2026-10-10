# Managed inference

This managed-mode contract is introduced in Steward 0.3.15.

Managed inference lets each user store an inference API key through Steward's
Connections UI. Steward encrypts the key at rest. Mint decrypts it only after a
live runtime proves its workload identity and owner binding, has model
authority, and has been reconciled with the managed-inference marker. Mint then
returns the user's long-lived key in the runtime's inference grant. The grant's
`expires_in` controls only the egress proxy's cache lifetime; it does not expire
or revoke the upstream key. The controller does not create a LiteLLM virtual
key in this mode.

Use `inference.mode=stock` when the controller should provision and revoke
runtime-scoped LiteLLM keys from a deployment master key. Use
`inference.mode=managed` only when the inference endpoint accepts the users'
stored keys.

## Database reader

Mint uses a separate PostgreSQL credential. It needs `CONNECT`, schema `USAGE`,
and `SELECT` on `managed_inference_credentials`; it does not run migrations or
read any other Steward table.

```sql
CREATE ROLE steward_mint_inference LOGIN;
GRANT CONNECT ON DATABASE steward TO steward_mint_inference;
GRANT USAGE ON SCHEMA public TO steward_mint_inference;
GRANT SELECT ON TABLE public.managed_inference_credentials TO steward_mint_inference;
```

Set the role's password through the database operator, then create the
`secrets.managedInferenceDatabase` Secret with a `url` value. Require the same
TLS verification as the main Steward database connection. Never put the URL or
password in Helm values.

Mint creates this pool lazily. If PostgreSQL is unavailable, inference grants
fail closed, while tool-token grants, JWKS, and introspection continue without
querying the database.

## Encryption key and chart values

Generate a random 32-byte deployment key outside the repository. Store it in
the dedicated Secret selected by `secrets.managedInferenceEncryptionKey`. The
chart projects only that file read-only into the apiserver and Mint; Mint's
signing and introspection material remains in its separate Secret. The managed
inference key is never projected into the controller, web UI, or agent runtime.

```yaml
inference:
  mode: managed
secrets:
  managedInferenceDatabase:
    name: steward-managed-inference-database
    key: url
  managedInferenceEncryptionKey:
    name: steward-managed-inference
    key: encryption-key
  mint:
    name: steward-mint
    signingKey: signing-key
    introspectionCredential: introspection-credential
networkPolicy:
  postgresCidrs:
    - 192.0.2.0/24
```

Keep `config.apiserver.inferenceEndpoint` set to the exact model operation URL.
The controller's LiteLLM management URL and `secrets.litellm` master key are not
mounted in managed mode. Ensure the PostgreSQL CIDR and port are admitted by
the Mint egress NetworkPolicy.

The upstream key's own LiteLLM limits govern models and budget. Steward checks
the Task's requested model during admission, but managed mode does not enforce
model or budget limits on each inference call and does not track inference
spend. Removing the key from Steward prevents future grants; it cannot revoke
the credential at the upstream service. If the key is exposed, it remains
usable after the runtime ends until the user or upstream administrator revokes
or rotates it.

After deployment, each user saves their own key under **Connections →
Inference / LLMs**. A model Task is rejected before reservation when the key is
missing or cannot be decrypted. The plaintext key is exposed only at Mint's
runtime inference-token response and the configured egress proxy, plus the
apiserver's transient decryptability check before Task reservation. None of
these components persists a second plaintext copy.

## Rotation

Rotate the Mint database password by updating only the managed-inference
database Secret and restarting Mint. Existing encrypted keys are unaffected.

The deployment encryption key cannot be replaced while old ciphertext remains.
For encryption-key rotation:

1. Stop new Task submissions and drain or revoke every managed runtime.
2. Have users remove their stored inference keys. Removal remains available in
   the Connections UI even after switching the chart to stock mode.
3. Replace the 32-byte Secret item and restart the apiserver and Mint.
4. Have users save their inference keys again, then submit one governed model
   Task and verify that only Mint and the egress proxy receive the key.
5. Re-enable normal submissions.

Do not retain the old key as an undocumented fallback or attempt to accept
multiple deployment keys.

## Rollback to stock mode

Rollback is a drain-and-revoke operation, not an in-place credential switch:

1. Stop new submissions, drain active Tasks, and revoke all managed runtimes.
2. Provision the stock LiteLLM management URL and master-key Secret.
3. Upgrade with `inference.mode=stock`.
4. Verify the controller provisions a new runtime-scoped key for a fresh Task.
5. Let users remove stored managed keys through the Connections UI.

Never switch a live runtime between managed and stock credentials. Steward
rejects a managed runtime that still carries a stock key reference so an
upstream credential cannot be orphaned.
