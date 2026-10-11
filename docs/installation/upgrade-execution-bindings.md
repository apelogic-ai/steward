# Upgrade note: explicit execution bindings

Every installation must configure `steward.execution-bindings/v2` before publishing or submitting
new versioned Workflows. Steward no longer synthesizes a coding agent, and v2 makes each binding's
supported model providers explicit.

## Existing active v1 catalogs

An installation already using `steward.execution-bindings/v1` must migrate the catalog as part of
the binary rollout. The new parser and Helm schema reject v1 catalogs; changing only the image or
only the catalog leaves one side unable to start.

1. Pause Task submission and Workflow publication.
2. Add `models.providers` to every binding. List only providers supported by that exact agent image
   and adapter deployment. Keep every other binding field unchanged.
3. Change the catalog `apiVersion` to `steward.execution-bindings/v2` and validate the complete Helm
   values against the new chart.
4. Roll out the new apiserver and controller images with that v2 catalog. Existing Tasks carrying a
   persisted v1 binding remain readable and continue from their immutable snapshot.
5. Wait until every old apiserver and controller pod is gone, then resume submission and
   publication. This prevents an old controller from claiming a newly admitted v2-bound Task.

To roll back, pause submission again and use the new controller to drain and finalize every Task
admitted with a v2 binding. Restore the previous binaries and the exact v1 catalog together, then
resume only after all pods are on the previous version. Previous binaries cannot read v2 binding
snapshots, while new binaries cannot start with a v1 catalog. Do not run a mixed controller fleet
while v2-bound Tasks are claimable.

## Installations without an explicit catalog

Installations that previously relied on an implicit coding-agent image or default logical agent
must create a complete v2 catalog before publishing or submitting new versioned Workflows.

This is an enforced two-stage rollout. Do not combine the stages: an old controller cannot safely
interpret a new bound Task, while a new controller must not invent a binding for an old unbound
Task.

Before rollout:

1. Inventory the exact logical references stored in executable Workflow revisions.
2. For each reference that must remain executable, configure its immutable image digest, absolute
   executable, exact version probe, supported model providers, and tool/inference provider-profile
   IDs and digests.
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

The v2 catalog adds required `models.providers` compatibility. Persisted v1 Task bindings remain
readable for their already-admitted immutable model, but new Task admission requires a v2 catalog.
