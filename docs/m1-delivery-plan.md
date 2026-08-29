# M1 delivery plan

Status: contract dependency index
Outcome: first catalog-backed governed Task through GitHub Actions

All M1 producers and consumers implement against the frozen
[steward.m1/v1 contract](contracts/m1/v1/README.md). It is checkpoint A and the sole
authority for public fields and authority decisions. Dependents must not publish or
consume provisional alternatives.

| Checkpoint | Exit criterion | Work |
|---|---|---|
| A — Contracts frozen | Request, identity, catalog, binding, evidence, revocation, and compatibility fixtures pass | M1-C01 |
| B — Catalog trusted | Identity ratifies source/scope/principal and Steward resolves one immutable package | M1-I01, M1-A01, M1-S02, M1-S03 |
| C — Admission authoritative | Immutable Envelope and native binding cover recomputed candidate; tool bindings are neutral | M1-G01, M1-S01, M1-S04, M1-S09 |
| D — Runtime auditable/revocable | Only admitted material reaches OpenShell, evidence verifies, revocation meets 60 seconds | M1-G02, M1-S05, M1-S06, M1-S07 |
| E — GHA path complete | Pinned steward-run v0.5.0 calls frozen contract without credential/runtime coupling | M1-R01, M1-S08, M1-S10, M1-G03 |
| M1 — Proven | Positive, negative, isolation, and revocation E2E pass against pinned stack | M1-E01, M1-E02, M1-E03 |

Target train: Steward v0.2.0, steward-run v0.5.0, and
Identity/github-oidc-exchange v0.4.0, with immutable release candidates for integration.
M1-C01 publishes no product tag. Dependencies unblock only after human merge and any
required immutable schema publication.

Required M1-C01 reviewers represent Identity, steward-run, GitOps, and agentic-ops.
Each review confirms implementation is possible from schema and fixtures without
adding a field or making a new authority decision.
