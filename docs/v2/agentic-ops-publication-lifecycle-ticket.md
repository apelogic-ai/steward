# DP-P01: Optional catalog publication and release provenance

Priority: post-P0

Status: safe to design after DP-C01; not required for the direct-package demo

## Goal

Add an optional curated catalog and release-provenance layer over immutable direct Git
packages. Publication improves discovery and operator-managed promotion; it is not a
prerequisite for source approval or direct execution.

## Why this remains separate

Direct invocation already binds a Task to stable repository identity, exact commit,
entry path, closure digest, and approved Envelope. Publication must not reintroduce an
opaque required server identity or make Git cease to be the package source of record.

## Scope

- validate an exact direct-package closure from an authorized source repository;
- publish an immutable human-friendly catalog coordinate that points to that exact
  source and closure digest;
- record the authorized publisher, source provenance, validation result, and
  publication witness;
- optionally verify an annotated tag or GitHub release as provenance metadata;
- prevent tag movement, coordinate replacement, and version-content substitution;
- expose deterministic catalog metadata for discovery; and
- define revocation and retention semantics without deleting historical Task evidence.

Automated publication may begin from repository CI after an authorized publisher
identity is established. A GitHub tag or release is evidence and discovery metadata,
not execution authority.

## Dependencies

- DP-C01 schema and closure rules;
- DP-G01 exact source retrieval;
- DP-S01 direct source/evidence model; and
- an explicit publisher authorization design.

It does not depend on the P0 local-main demo being run and can be developed in a
separate light lane once the direct-package contracts are stable.

## Negative tests

- a mutable branch or tag cannot replace an exact commit;
- moving or recreating a tag cannot change an existing catalog coordinate;
- a publisher cannot publish from an unauthorized repository;
- a claimed digest mismatch fails before a witness is recorded;
- publication never widens an Envelope or provider authority; and
- catalog revocation does not corrupt retained execution evidence.

## Exit criteria

- one exact direct package can be published automatically under an authorized
  publisher;
- its optional release/tag provenance is verifiable and immutable;
- the direct Git path continues to execute without publication; and
- frozen `steward.m1/v1` behavior is unchanged.

## Parallel safety

Safe to run in parallel with conformance work after DP-C01 and the source/evidence
interfaces stabilize. Avoid concurrent changes to shared schemas or heavy integration
lanes; coordinate those through their owning tickets.
