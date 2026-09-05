# Upgrade note: explicit execution bindings

Installations that previously relied on an implicit coding-agent image or default logical agent
must configure `steward.execution-bindings/v1` before publishing or submitting new versioned
Workflows. Steward no longer synthesizes a coding agent.

This is an enforced two-stage rollout. Do not combine the stages: an old controller cannot safely
interpret a new bound Task, while a new controller must not invent a binding for an old unbound
Task.

Before rollout:

1. Inventory the exact logical references stored in executable Workflow revisions.
2. For each reference that must remain executable, configure its immutable image digest, absolute
   executable, exact version probe, and tool/inference provider-profile IDs and digests.
3. Install those immutable OpenShell profiles under the configured IDs.
4. Run the digest-pinned offline container validator from
   [the installation contract](execution-bindings.md#catalog-schema) and retain the printed derived
   identities with the deployment review.
5. Stop versioned Workflow submission and finalize every existing versioned Task. Migration 0026
   takes a write lock and refuses to apply while any pre-binding versioned Task is not finalized.
6. First upgrade every apiserver and controller with `executionBindingsMode: staged`. The migration
   then rejects new unbound versioned rows from an old apiserver, while staged new apiservers
   advertise no agents and reject versioned publication/submission. Wait for both Deployments to
   complete; do not activate while any old controller can claim work.
7. In a second Helm upgrade, keep the exact tested images and catalog and change only
   `executionBindingsMode: active`. Versioned publication and submission resume with complete
   persisted bindings.

Historical Workflow revisions and finalized legacy Task rows are not rewritten. Existing Task rows that already
contain a binding continue from that snapshot. Rows without a binding remain readable and do not
receive a manufactured binding. New submissions for a Workflow whose reference is absent fail
before reservation. Roll back first to `staged`, drain and finalize all bound Tasks, and only then
restore old binaries. Never run old controllers while bound Tasks are claimable.
