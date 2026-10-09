# Managed inference credentials

Steward supports two deployment-wide inference credential modes:

- `stock` (the default) uses the LiteLLM management API and its master key to
  create a bounded credential for each governed runtime.
- `managed` stores one user-supplied inference gateway API key for each Steward
  user. The upstream gateway owns the key's lifetime and budget; Steward does
  not issue, inspect, refresh, freeze, or revoke it.

The mode is an operator choice, not a Task or user setting. Envelopes continue
to select the models a Task may use in either mode. In managed v1, Steward does
not rewrite the user-supplied key's upstream model allowlist: the LiteLLM key
must independently permit the Envelope-selected models. A narrower upstream
allowlist fails the model call without widening Steward authority.

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
When `databaseTls.mode=verify-full`, that URL must verify the database hostname
and use the mounted CA at `/run/database-tls/ca.crt`.

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

Before switching an existing `stock` deployment to `managed`:

1. Stop new Task submissions while keeping the current controller and LiteLLM
   master key available.
2. Let every active Task finish or cancel it, and let the controller complete
   runtime finalization. `kubectl get agentruntimes.agents.apelogic.ai -A` must
   report no remaining runtime. Verify LiteLLM has no key whose metadata carries
   a `steward_runtime_uid`; this is the upstream revocation proof.
3. If either check fails, remain in `stock` mode and repair finalization. Managed
   mode deliberately rejects a leftover runtime credential instead of deleting
   the Kubernetes Secret and orphaning its upstream LiteLLM key.
4. Back up PostgreSQL, record the current migration head and Helm revision, and
   create the deployment-key Secret.
5. Roll the complete Steward release with `inference.mode=managed`, then reopen
   submissions. Users must add keys before model Tasks can be admitted.

Migration 0068 is additive and remains in place during rollback. To return to
`stock`, first stop new submissions and let active Tasks finish, restore the
LiteLLM master-key Secret and management URL, set `inference.mode=stock`, and
roll the complete release. The older binary ignores the additive tables, but
stored managed credentials remain encrypted and unused. Remove credentials
through Steward before rollback when policy requires their destruction; do
not delete rows or audit history manually.
