# Managed inference credentials

Steward supports two deployment-wide inference credential modes:

- `stock` (the default) uses the LiteLLM management API and its master key to
  create a bounded credential for each governed runtime.
- `managed` stores one user-supplied inference gateway API key for each Steward
  user. The upstream gateway owns the key's lifetime and budget; Steward does
  not issue, inspect, refresh, freeze, or revoke it.

The mode is an operator choice, not a Task or user setting. Envelopes continue
to constrain the models a Task may use in either mode.

## Configure managed mode

Create a deployment key containing exactly 32 random bytes. Keep it in the
release namespace and outside Helm values and source control:

```sh
set +x
umask 077
install -d -m 0700 ./private
openssl rand 32 > ./private/managed-inference-encryption-key
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward create secret generic steward-managed-inference \
  --from-file=encryption-key=./private/managed-inference-encryption-key \
  --dry-run=client -o yaml | \
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" apply -f -
```

Select the mode and the existing Secret:

```yaml
inference:
  mode: managed
  managed:
    databaseSecret:
      name: steward-managed-inference-database
      key: url
    encryptionKeySecret:
      name: steward-managed-inference
      key: encryption-key
```

`databaseSecret` is a separate PostgreSQL credential used only by Mint to read
the encrypted credential row for the verified runtime owner. Grant it only
`CONNECT`, schema `USAGE`, and `SELECT` on
`managed_inference_credentials`; do not reuse `secrets.database`.

Managed mode does not require `secrets.litellm` or
`config.controller.litellmUrl`. The API and Mint mount the deployment key;
the controller does not receive a LiteLLM master key and does not create
runtime inference credentials. Mint resolves the runtime's verified canonical
owner and decrypts that user's saved key only for the inference egress proxy.
The agent process does not receive the key in its environment, files, or
arguments.

Each user opens **Connections → Inference / LLMs** and adds their key. Steward
returns only its last four characters and save time. **Replace key** overwrites
Steward's encrypted copy; **Remove key** destroys Steward's copy but does not
revoke the upstream key. A model Task without a saved key fails before
reservation with `inference_key_missing`.

## Storage and backup

Migration `0068_managed_inference_credentials.sql` adds the encrypted
per-user credential table and an append-only add/replace/remove audit table.
Each credential has a random data-encryption key; that key is wrapped by the
32-byte deployment key with the canonical user ID as authenticated associated
data. The database never stores the deployment key or plaintext credential.

Back up the database and the deployment-key Secret through the platform's
approved secret-backup mechanism as one recovery unit. A database restore
without the matching deployment key makes every stored credential unusable.
Do not rotate or delete the deployment key until a separately supported
rewrap procedure exists; users can instead replace their individual keys.

## Upgrade and rollback

Before switching an existing `stock` deployment to `managed`, back up
PostgreSQL, record the current migration head and Helm revision, create the
deployment-key Secret, and roll the complete Steward release with
`inference.mode=managed`. Users must add keys before their model Tasks can be
admitted.

Migration 0068 is additive and remains in place during rollback. To return to
`stock`, first stop new submissions and let active Tasks finish, restore the
LiteLLM master-key Secret and management URL, set `inference.mode=stock`, and
roll the complete release. The older binary ignores the additive tables, but
stored managed credentials remain encrypted and unused. Remove credentials
through Steward before rollback when policy requires their destruction; do
not delete rows or audit history manually.
