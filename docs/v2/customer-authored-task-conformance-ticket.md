# DP-T01: Customer-authored direct Task conformance

Priority: post-P0

Status: test design may start after DP-C01; execution depends on direct-package releases

## Goal

Extend the single P0 demonstration into a durable conformance suite for independently
authored direct packages and workflows.

The successful authorized cross-repository case belongs to P0. This ticket adds the
breadth, isolation, denial, and repeatability coverage intentionally deferred from the
demo deadline.

## Scope

- multiple packages at independent exact commits;
- multiple caller workflows and reusable-workflow pins;
- same-repository `git:trigger` and exact cross-repository references;
- source-binding revocation and repository-identity mismatch;
- explicit narrower `requires` and over-Envelope denial;
- tool and model isolation across packages and workflows;
- deterministic closure/evidence behavior under retry;
- diagnostics off versus successful-run full transcript;
- stable-lane repetition, upgrade, rollback, and retained-state checks; and
- negative checks for unauthorized provider, source, Envelope, input, and output
  substitution.

## Required scenarios

1. Two packages can remain in use at different exact commits without either becoming
   the implicit latest version.
2. Two workflows using different packages cannot inherit each other's tools, models,
   outputs, source bindings, or idempotency identities.
3. A same-repository `git:trigger` resolves only to the ratified caller commit.
4. An unauthorized cross-repository source fails before Task reservation.
5. A narrower explicit request is admitted; an over-Envelope request follows normal
   Steward denial or instance-bound grant semantics without mutating the Envelope.
6. Revoking a source or Envelope binding prevents new Tasks while retained evidence
   for old Tasks remains interpretable.
7. Full diagnostics are returned only for opted-in successful Tasks and are absent
   otherwise.
8. Repeated stable-lane runs preserve source, closure, authority, runtime, provider,
   output, and cleanup isolation.

## Test discipline

- negative escape attempts precede implementation fixes;
- mocks are limited to units that cannot reasonably use the real provider in CI;
- at least one protected lane uses real GitHub MCP calls through MCP-GW;
- the stable/main lane is operated only by GitOps and is never a development testbed;
- heavy local integration lanes are serialized; and
- all resources and credentials follow the repository's run ownership and cleanup
  rules.

## Exit criteria

- every required scenario has a named automated test and evidence assertion;
- stable-lane runs pass repeatedly across the supported upgrade boundary;
- a failure identifies source, admission, execution, provider, output, or cleanup
  stage without exposing credentials; and
- frozen v1 and the P0 direct-package path remain compatible.

## Parallel safety

Fixture and assertion design can proceed alongside DP-P01 after DP-C01. Runtime E2E
implementation waits for DP-I01, DP-G01, DP-S01, and DP-R01 releases. Publication and
conformance may run in parallel when they avoid the same schema files and do not run
heavy integration lanes concurrently.
